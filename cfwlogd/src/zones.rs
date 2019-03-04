// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

// Copyright 2019 Joyent, Inc.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crossbeam::sync::ShardedLock;
use vminfod::{Changes, VminfodEvent, Zone};

pub type Vmobjs = Arc<ShardedLock<HashMap<Zonedid, Zone>>>;
pub type Zonedid = u32;

/// Inserts or updates an existing vmobj into a given `Vmobjs`
fn insert_vmobj(zone: Zone, vmobjs: Vmobjs) {
    let mut w = vmobjs.write().unwrap();
    w.insert(zone.zonedid, zone);
}

/// Search through a vminfod changes payload and see if the alias was apart of the update
fn alias_changed(changes: &[Changes]) -> bool {
    for change in changes {
        if change
            .path
            .first()
            // double map_or because path is a `Vec<Option<String>>`
            .map_or(false, |v| v.as_ref().map_or(false, |a| a == "alias"))
        {
            return true;
        }
    }
    false
}

/// Start a vminfod watcher thread that will keep a `Vmobjs` object up-to-date.
/// This function will block until the spawned thread has processed the `Ready` event from vminfod
pub fn start_vminfod(vmobjs: Vmobjs) -> thread::JoinHandle<()> {
    #[allow(clippy::mutex_atomic)] // this lint doesn't realize we are using it with a CondVar
    let waiter = Arc::new((Mutex::new(false), Condvar::new()));
    let waiter2 = Arc::clone(&waiter);
    let handle = thread::Builder::new()
        .name("vminfod_event_processor".to_owned())
        .spawn(move || {
            info!("starting vminfod thread");
            let &(ref lock, ref cvar) = &*waiter2;
            let (r, _) = vminfod::start_vminfod_stream();
            for event in r.iter() {
                match event {
                    VminfodEvent::Ready(event) => {
                        let raw_vms = event.vms;
                        let mut ready = lock.lock().unwrap();
                        // Make sure we don't see another ready event in the future
                        assert_eq!(*ready, false);
                        let vms: Vec<Zone> = serde_json::from_str(&raw_vms)
                            .expect("failed to parse vms payload from vminfod");
                        let mut w = vmobjs.write().unwrap();
                        for vm in vms {
                            w.insert(vm.zonedid, vm);
                        }
                        *ready = true;
                        debug!("vminfod ready event processed");
                        cvar.notify_one();
                    }
                    VminfodEvent::Create(event) => insert_vmobj(event.vm, Arc::clone(&vmobjs)),
                    VminfodEvent::Modify(event) => {
                        if alias_changed(&event.changes) {
                            debug!(
                                "alias changed for {} ({}), updating vmobj mapping",
                                &event.vm.uuid, &event.vm.zonedid
                            );
                            insert_vmobj(event.vm, Arc::clone(&vmobjs));
                        }
                    }
                    // Nothing to be done with deletes currently. We don't modify `Vmobjs` since
                    // cfw event logs in various processing queues may not have made it to disk
                    // yet. We may eventually want to signal a logger that it's okay to shutdown.
                    VminfodEvent::Delete(_) => (),
                }
            }
            // TODO TRITON-1754: implement retry logic here, until then just panic
            panic!("vminfod event stream closed");
        })
        .expect("vminfod client thread spawn failed.");

    let &(ref lock, ref cvar) = &*waiter;
    let mut ready = lock.lock().unwrap();
    while !*ready {
        ready = cvar.wait(ready).unwrap();
    }

    handle
}

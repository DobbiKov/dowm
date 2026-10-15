//! Resolve `--app <name|pid>` to a running process.

use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

use crate::appspec::{AppSpec, pick_by_name};

pub fn resolve(spec: &AppSpec) -> Result<(i32, String), String> {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    let regular: Vec<(i32, String)> = apps
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .map(|a| (a.processIdentifier(), a.localizedName().map(|n| n.to_string()).unwrap_or_default()))
        .collect();
    match spec {
        AppSpec::Pid(pid) => Ok(regular
            .iter()
            .find(|(p, _)| p == pid)
            .cloned()
            .unwrap_or_else(|| (*pid, format!("pid {pid}")))),
        AppSpec::Name(name) => {
            let names: Vec<String> = regular.iter().map(|(_, n)| n.clone()).collect();
            pick_by_name(name, &names).map(|i| regular[i].clone())
        }
    }
}

use std::process::{Command, Output};

#[derive(Debug, PartialEq, Eq)]
pub enum ServiceState {
    Unknown(String),
    Details(ServiceDetails),
}

impl ServiceState {
    fn from(name: &str, maybe_output: Option<Output>) -> Self {
        // TODO: replace with try {} when available
        // https://github.com/rust-lang/rust/issues/31436
        (|| {
            let output = maybe_output?;
            let status = String::from_utf8(output.stdout).ok()?;
            let active = output.status.success();
            Some(ServiceState::Details(ServiceDetails {
                name: name.to_string(),
                status,
                active,
            }))
        })()
        .unwrap_or_else(|| ServiceState::Unknown(name.to_string()))
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Unknown(name) => name,
            Self::Details(ServiceDetails { name, .. }) => name,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ServiceDetails {
    name: String,
    pub active: bool,
    pub status: String,
}

pub fn update_service_status(current: Option<&ServiceState>) -> Option<ServiceState> {
    current.map(|s| service_status(s.name()))
}

pub fn service_status(unit: &str) -> ServiceState {
    let output = Command::new("systemctl")
        .args(["is-active", unit])
        .output()
        .ok();

    ServiceState::from(unit, output)
}

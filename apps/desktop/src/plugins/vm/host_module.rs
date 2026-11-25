//! Native completion payloads contain no `QuickJS` values and never cross sessions.
#[derive(Debug)]
pub(super) struct HostDone {
    pub sequence: u64,
    pub result: Result<String, HostError>,
}
#[derive(Debug)]
pub(super) struct HostError {
    pub message: String,
    pub status: Option<u16>,
}
impl From<crate::Error> for HostError {
    fn from(error: crate::Error) -> Self {
        let (message, status) = match error {
            crate::Error::HttpStatus { status, .. } => (
                format!("HTTP {status}: upstream request failed"),
                Some(status),
            ),
            crate::Error::Cancelled => ("run was cancelled".into(), None),
            crate::Error::Config(ref message) if message.contains("cannot access host") => {
                ("ERR_ACCESS_DENIED".into(), None)
            }
            _ => ("plugin host call failed".into(), None),
        };
        Self { message, status }
    }
}

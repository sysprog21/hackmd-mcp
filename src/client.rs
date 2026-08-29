/// HTTP client shared by all `HackMD` tool handlers.
///
/// Request configuration and API operations are added in the following P0
/// tasks. Keeping this type private prevents the transport layer from becoming
/// part of the crate's public API.
#[derive(Debug, Default)]
pub(crate) struct HackmdClient;

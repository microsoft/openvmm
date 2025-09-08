use zerocopy::IntoBytes;

pub mod tpm_protocol;

pub struct TpmUtil;
impl TpmUtil {
    pub fn get_self_test_cmd() -> [u8; 4096] {
        let session_tag = tpm_protocol::SessionTagEnum::NoSessions;
        let cmd = tpm_protocol::protocol::SelfTestCmd::new(session_tag.into(), true);
        let mut buffer = [0; 4096];
        buffer[..cmd.as_bytes().len()].copy_from_slice(cmd.as_bytes());
        buffer
    }
}

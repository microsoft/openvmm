use zerocopy::IntoBytes;

pub mod protocol;

pub struct TpmUtil;
impl TpmUtil {
    pub fn get_self_test_cmd() -> [u8; 4096] {
        let session_tag = protocol::SessionTagEnum::NoSessions;
        let cmd = protocol::protocol::SelfTestCmd::new(session_tag.into(), true);
        let mut buffer = [0; 4096];
        buffer[..cmd.as_bytes().len()].copy_from_slice(cmd.as_bytes());
        buffer
    }
}

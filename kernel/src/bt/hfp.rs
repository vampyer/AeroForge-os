//! The audio gateway side of the Hands-Free and Headset profiles: AT
//! commands from the headset arrive on RFCOMM and get answered as a phone
//! with no call in progress would. Hands-free needs a "service level
//! connection" (features, indicators, event reporting) before voice; the
//! older Headset profile only has the button press, so it is ready at once.

use alloc::string::String;
use alloc::vec::Vec;

/// The indicators we report: service on, no call, full signal and battery.
const CIND_RANGES: &str = "+CIND: (\"service\",(0,1)),(\"call\",(0,1)),(\"callsetup\",(0-3)),(\"callheld\",(0-2)),\
(\"signal\",(0-5)),(\"roam\",(0,1)),(\"battchg\",(0-5))";
const CIND_VALUES: &str = "+CIND: 1,0,0,0,5,0,5";

pub struct Ag {
    line: String,
    /// The service level connection is up: voice can start.
    pub ready: bool,
}

impl Ag {
    /// `headset`: the Headset profile, which has no service level connection.
    pub fn new(headset: bool) -> Self {
        Ag { line: String::new(), ready: headset }
    }

    /// Data from the headset; returns the replies to send.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        for &b in data {
            match b {
                b'\r' | b'\n' => {
                    let line = core::mem::take(&mut self.line);
                    if !line.trim().is_empty() {
                        self.command(line.trim(), &mut replies);
                    }
                }
                _ if self.line.len() < 256 => self.line.push(b as char),
                _ => {}
            }
        }
        replies
    }

    fn command(&mut self, cmd: &str, replies: &mut Vec<Vec<u8>>) {
        let upper = cmd.to_ascii_uppercase();
        let say = |replies: &mut Vec<Vec<u8>>, s: &str| replies.push(alloc::format!("\r\n{}\r\n", s).into_bytes());
        match upper.as_str() {
            // Our features: none of the optional ones (no three-way
            // calling, no codec negotiation: plain CVSD voice).
            c if c.starts_with("AT+BRSF=") => say(replies, "+BRSF: 0"),
            "AT+CIND=?" => say(replies, CIND_RANGES),
            "AT+CIND?" => say(replies, CIND_VALUES),
            c if c.starts_with("AT+CMER=") => {
                say(replies, "OK");
                self.ready = true;
                return;
            }
            "AT+CHLD=?" => say(replies, "+CHLD: (0,1,2,3)"),
            "AT+COPS?" => say(replies, "+COPS: 0,0,\"AeroForge\""),
            "AT+CNUM" | "AT+CLCC" | "AT+BTRH?" => {}
            c if ["AT+CKPD", "AT+VGS", "AT+VGM", "AT+CMEE", "AT+CLIP", "AT+CCWA", "AT+NREC", "AT+BIA", "AT+XAPL",
                "AT+IPHONEACCEV", "AT+COPS=", "AT+BAC", "AT+XEVENT", "AT+CSRSF", "AT+APLSIRI", "AT+BCC"]
                .iter().any(|p| c.starts_with(p)) => {}
            _ => {
                say(replies, "ERROR");
                return;
            }
        }
        say(replies, "OK");
    }
}

//! A node's hardware address.
use std::fmt;

/// The hardware address of a node, which stays the same when the network
/// gives the node a different IP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mac([u8; 6]);

impl Mac {
    /// Read a hardware address written as twelve hex digits, with or without
    /// separators, in either case.
    pub fn parse(mac: &str) -> Option<Self> {
        let mut digits = mac
            .chars()
            .filter(|c| !matches!(c, ':' | '-' | '.') && !c.is_whitespace());
        let mut octets = [0; 6];
        for octet in &mut octets {
            let high = digits.next()?.to_digit(16)?;
            let low = digits.next()?.to_digit(16)?;
            *octet = u8::try_from(high * 16 + low).ok()?;
        }
        digits.next().is_none().then_some(Self(octets))
    }

    /// The six bytes of the address.
    pub fn octets(&self) -> [u8; 6] {
        self.0
    }
}

/// Written as WLED writes it: twelve lowercase hex digits.
impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for octet in self.0 {
            write!(f, "{octet:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_is_read_however_it_is_written() {
        let mac = Mac::parse("a842e38d9828").unwrap();
        assert_eq!(mac.octets(), [0xa8, 0x42, 0xe3, 0x8d, 0x98, 0x28]);
        assert_eq!(Mac::parse("A8:42:E3:8D:98:28"), Some(mac));
        assert_eq!(Mac::parse("a8-42-e3-8d-98-28"), Some(mac));
        assert_eq!(mac.to_string(), "a842e38d9828");
    }

    #[test]
    fn anything_else_is_not_an_address() {
        assert_eq!(Mac::parse(""), None);
        assert_eq!(Mac::parse("a842e38d98"), None);
        assert_eq!(Mac::parse("a842e38d982800"), None);
        assert_eq!(Mac::parse("not a mac!!!"), None);
    }
}

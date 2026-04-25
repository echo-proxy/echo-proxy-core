use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
};

#[derive(Debug)]
pub enum GeoIpError {
    Io(std::io::Error),
    Decode(String),
    TagNotFound(String),
    InvalidCidr { ip_len: usize },
}

impl std::fmt::Display for GeoIpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read geoip dat: {e}"),
            Self::Decode(msg) => write!(f, "failed to decode geoip dat: {msg}"),
            Self::TagNotFound(tag) => write!(f, "tag '{tag}' not found in geoip dat"),
            Self::InvalidCidr { ip_len } => write!(f, "unexpected IP byte length: {ip_len}"),
        }
    }
}

impl std::error::Error for GeoIpError {}

impl From<std::io::Error> for GeoIpError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Load a V2Ray `geoip.dat` file and extract CIDR ranges for the given tags.
///
/// Tags are compared case-insensitively against the `country_code` field.
/// Returns an error if the file is missing, cannot be decoded, or any tag is
/// not present in the dat file.
pub fn load_geoip_dat(path: &Path, tags: &[String]) -> Result<Vec<IpNet>, GeoIpError> {
    let bytes = std::fs::read(path)?;

    let list = geosite_rs::decode_geoip(&bytes).map_err(|e| GeoIpError::Decode(e.to_string()))?;

    let mut result = Vec::new();

    for tag in tags {
        let tag_upper = tag.to_uppercase();
        let entry = list
            .entry
            .iter()
            .find(|e| e.country_code.to_uppercase() == tag_upper)
            .ok_or_else(|| GeoIpError::TagNotFound(tag.clone()))?;

        for cidr in &entry.cidr {
            let net = match cidr.ip.len() {
                4 => {
                    let addr = Ipv4Addr::from(<[u8; 4]>::try_from(cidr.ip.as_slice()).unwrap());
                    IpNet::V4(
                        Ipv4Net::new(addr, cidr.prefix as u8)
                            .map_err(|_| GeoIpError::InvalidCidr { ip_len: 4 })?,
                    )
                }
                16 => {
                    let addr = Ipv6Addr::from(<[u8; 16]>::try_from(cidr.ip.as_slice()).unwrap());
                    IpNet::V6(
                        Ipv6Net::new(addr, cidr.prefix as u8)
                            .map_err(|_| GeoIpError::InvalidCidr { ip_len: 16 })?,
                    )
                }
                n => return Err(GeoIpError::InvalidCidr { ip_len: n }),
            };
            result.push(net);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn missing_file_returns_io_error() {
        let err = load_geoip_dat(Path::new("nonexistent.dat"), &["cn".to_string()]).unwrap_err();
        assert!(matches!(err, GeoIpError::Io(_)));
    }
}

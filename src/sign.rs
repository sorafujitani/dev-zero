//! SSH signatures shared by the GitHub-key posting endpoint and signed API requests.

use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

/// Unix time in seconds, as the API expects in `X-Devnull-Timestamp`.
pub fn timestamp() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// 32 random hex characters, used once per signature.
pub fn nonce() -> Result<String> {
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")
        .context("could not open the system random source")?
        .read_exact(&mut random)
        .context("could not generate a signing nonce")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Signs `payload` with `ssh-keygen -Y sign` and returns the SSHSIG as one base64 line.
///
/// Signing may ask for a passphrase on the terminal, so callers in raw mode leave it first.
pub fn ssh_sign(namespace: &str, payload: &str, key: &Path) -> Result<String> {
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-q", "-n", namespace, "-f"])
        .arg(key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not start ssh-keygen; install OpenSSH 8.2 or newer")?;
    let written = child
        .stdin
        .take()
        .context("ssh-keygen stdin unavailable")?
        .write_all(payload.as_bytes());
    let signed = child
        .wait_with_output()
        .context("could not wait for ssh-keygen")?;
    if !signed.status.success() {
        bail!("SSH signing failed; check the key path and your SSH agent");
    }
    written.context("could not send message to ssh-keygen")?;
    signature_line(&signed.stdout)
}

fn signature_line(pem: &[u8]) -> Result<String> {
    let pem = std::str::from_utf8(pem)
        .context("ssh-keygen returned invalid UTF-8")?
        .trim();
    let signature: String = pem
        .strip_prefix("-----BEGIN SSH SIGNATURE-----")
        .and_then(|pem| pem.strip_suffix("-----END SSH SIGNATURE-----"))
        .context("ssh-keygen returned an invalid SSH signature")?
        .lines()
        .map(str::trim)
        .collect();
    if signature.is_empty()
        || !signature
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
    {
        bail!("ssh-keygen returned an invalid signature encoding");
    }
    Ok(signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_valid_signature_lines() {
        assert_eq!(
            signature_line(
                b"-----BEGIN SSH SIGNATURE-----\nYWJj\nZA==\n-----END SSH SIGNATURE-----\n"
            )
            .unwrap(),
            "YWJjZA=="
        );
        for pem in [
            "not a signature",
            "-----BEGIN SSH SIGNATURE-----\n-----END SSH SIGNATURE-----",
            "-----BEGIN SSH SIGNATURE-----\nabc:injection\n-----END SSH SIGNATURE-----",
        ] {
            assert!(signature_line(pem.as_bytes()).is_err());
        }
    }

    #[test]
    fn nonces_are_fresh_hex() {
        let (a, b) = (nonce().unwrap(), nonce().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}

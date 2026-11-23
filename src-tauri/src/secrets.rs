//! The API token, kept in the Windows Credential Manager through the `keyring` crate. It never goes in
//! a file, a log line or back to the UI once saved (the UI is only told whether there is one).
//!
//! In Credential Manager it shows under "Windows Credentials" as `sotalore-companion`, one per site URL,
//! so a test server's token doesn't replace the real one.

const SERVICE: &str = "sotalore-companion";

fn entry(site: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, site).map_err(|e| format!("Credential Manager: {e}"))
}

/// The saved token for a site, or `None` if there isn't one.
pub fn get_token(site: &str) -> Result<Option<String>, String> {
    match entry(site)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Couldn't read the token from Credential Manager: {e}")),
    }
}

pub fn set_token(site: &str, token: &str) -> Result<(), String> {
    entry(site)?
        .set_password(token)
        .map_err(|e| format!("Couldn't save the token in Credential Manager: {e}"))
}

pub fn clear_token(site: &str) -> Result<(), String> {
    match entry(site)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Couldn't remove the token from Credential Manager: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses the real Credential Manager, under a site name no real setting would have, and cleans up.
    #[test]
    fn saves_reads_and_removes_a_token() {
        let site = "https://round-trip-test.invalid";
        clear_token(site).unwrap();
        assert_eq!(get_token(site).unwrap(), None);
        set_token(site, "slt_round_trip").unwrap();
        assert_eq!(get_token(site).unwrap().as_deref(), Some("slt_round_trip"));
        clear_token(site).unwrap();
        assert_eq!(get_token(site).unwrap(), None);
    }
}

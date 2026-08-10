from pathlib import Path


def replace_exact(path: Path, old: str, new: str, label: str) -> None:
    source = path.read_text()
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    path.write_text(source.replace(old, new))


auth = Path("src/auth.rs")
replace_exact(
    auth,
    """                let credential = selected_customer_credential(headers)?;
                authority
                    .authenticate(&credential.token, credential.cookie_authenticated)
""",
    """                let credential = match presented_credential(headers) {
                    CredentialSelection::Valid(credential) => credential,
                    CredentialSelection::Missing => {
                        return Err(deny(StatusCode::UNAUTHORIZED, "missing_customer_session"));
                    }
                    CredentialSelection::Invalid => {
                        return Err(deny(StatusCode::UNAUTHORIZED, "invalid_customer_session"));
                    }
                };
                authority
                    .authenticate(&credential.token, credential.cookie_authenticated)
""",
    "inline credential selection",
)
replace_exact(
    auth,
    """fn selected_customer_credential(headers: &HeaderMap) -> Result<PresentedCredential, Response> {
    match presented_credential(headers) {
        CredentialSelection::Valid(credential) => Ok(credential),
        CredentialSelection::Missing => {
            Err(deny(StatusCode::UNAUTHORIZED, "missing_customer_session"))
        }
        CredentialSelection::Invalid => {
            Err(deny(StatusCode::UNAUTHORIZED, "invalid_customer_session"))
        }
    }
}

""",
    "",
    "remove oversized Result helper",
)
replace_exact(
    auth,
    """pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    match presented_credential(headers) {
        CredentialSelection::Valid(credential) => Some(credential.token),
        CredentialSelection::Missing | CredentialSelection::Invalid => None,
    }
}

""",
    """#[cfg(test)]
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    match presented_credential(headers) {
        CredentialSelection::Valid(credential) => Some(credential.token),
        CredentialSelection::Missing | CredentialSelection::Invalid => None,
    }
}

""",
    "limit bearer helper to parser tests",
)

login = Path("src/login.rs")
replace_exact(
    login,
    """pub(crate) fn clear_customer_provider_session_cookie() -> String {
    format!(
        "{CUSTOMER_PROVIDER_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        cookie_secure_suffix()
    )
}

""",
    "",
    "remove unused provider-cookie helper",
)

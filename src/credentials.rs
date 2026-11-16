//! Platform credentials, read from files of a mounted Kubernetes Secret.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// A secret string. Never printed: `Debug` is redacted and there is no `Display`.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Username and password for a tipping platform.
pub struct Credentials {
    username: Secret,
    password: Secret,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum CredentialsError {
    #[error("credential '{0}' is empty")]
    Empty(&'static str),
    #[error("could not read credentials file {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
}

/// Removes exactly one trailing `\n` or `\r\n` (as left by `echo` or editors).
fn trim_one_newline(mut s: String) -> String {
    if s.ends_with("\r\n") {
        s.truncate(s.len() - 2);
    } else if s.ends_with('\n') {
        s.truncate(s.len() - 1);
    }
    s
}

impl Credentials {
    pub fn from_parts(username: String, password: String) -> Result<Self, CredentialsError> {
        let username = trim_one_newline(username);
        let password = trim_one_newline(password);
        if username.is_empty() {
            return Err(CredentialsError::Empty("username"));
        }
        if password.is_empty() {
            return Err(CredentialsError::Empty("password"));
        }
        Ok(Self {
            username: Secret(username),
            password: Secret(password),
        })
    }

    pub fn username(&self) -> &Secret {
        &self.username
    }

    pub fn password(&self) -> &Secret {
        &self.password
    }
}

async fn read_file(path: PathBuf) -> Result<String, CredentialsError> {
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|source| CredentialsError::Read { path, source })
}

/// Loads `username` and `password` from files in `dir`.
pub async fn load(dir: &Path) -> Result<Credentials, CredentialsError> {
    let username = read_file(dir.join("username")).await?;
    let password = read_file(dir.join("password")).await?;
    Credentials::from_parts(username, password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_one_trailing_newline() {
        let c = Credentials::from_parts("test-user\n".into(), "hunter2\r\n".into()).unwrap();
        assert_eq!(c.username().expose(), "test-user");
        assert_eq!(c.password().expose(), "hunter2");

        let c = Credentials::from_parts("test-user\n\n".into(), "hunter2 ".into()).unwrap();
        assert_eq!(c.username().expose(), "test-user\n");
        assert_eq!(c.password().expose(), "hunter2 ");
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            Credentials::from_parts("\n".into(), "hunter2".into()),
            Err(CredentialsError::Empty("username"))
        ));
        assert!(matches!(
            Credentials::from_parts("test-user".into(), String::new()),
            Err(CredentialsError::Empty("password"))
        ));
    }

    #[test]
    fn debug_is_redacted() {
        let c = Credentials::from_parts("test-user".into(), "hunter2".into()).unwrap();
        for s in [
            format!("{c:?}"),
            format!("{:?}", c.username()),
            format!("{:?}", c.password()),
        ] {
            assert!(!s.contains("test-user"), "{s}");
            assert!(!s.contains("hunter2"), "{s}");
        }
    }

    #[tokio::test]
    async fn load_nonexistent_dir_reports_path() {
        let dir = Path::new("/nonexistent/betbot-test");
        let err = load(dir).await.unwrap_err();
        match &err {
            CredentialsError::Read { path, .. } => {
                assert_eq!(path, &dir.join("username"));
            }
            other => panic!("unexpected error {other:?}"),
        }
        assert!(
            err.to_string()
                .contains("/nonexistent/betbot-test/username")
        );
    }
}

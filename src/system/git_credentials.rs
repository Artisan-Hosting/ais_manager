use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoCredential {
    Header { url_prefix: String, header: String },
    Anonymous,
    Pending(String),
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize]);
        out.push(ALPHABET[((n >> 12) & 63) as usize]);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize]);
        } else {
            out.push(b'=');
        }
    }
    String::from_utf8(out).unwrap()
}

fn url_scheme_host_port(remote_url: &str) -> Option<(String, String)> {
    let (scheme, after) = remote_url.split_once("://")?;
    if scheme.is_empty() || after.is_empty() {
        return None;
    }
    let host_port = after
        .trim_start_matches("//")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if host_port.is_empty() {
        return None;
    }
    Some((scheme.to_string(), host_port.to_string()))
}

fn is_github(host: &str) -> bool {
    host == "github.com" || host.ends_with(".github.com")
}

pub fn resolve_repo_credential(
    token: Option<&str>,
    id: &str,
    remote_url: &str,
    token_dir: &Path,
    global_token: Option<&str>,
) -> RepoCredential {
    let (scheme, host_port) = match url_scheme_host_port(remote_url) {
        Some(v) => v,
        None => return RepoCredential::Anonymous,
    };
    let url_prefix = format!("{}://{}/", scheme, host_port);

    match token {
        None => {
            if is_github(&host_port) {
                if let Some(gt) = global_token {
                    RepoCredential::Header {
                        url_prefix,
                        header: format!(
                            "Authorization: Basic {}",
                            base64(format!("x-access-token:{}", gt).as_bytes())
                        ),
                    }
                } else {
                    RepoCredential::Anonymous
                }
            } else {
                RepoCredential::Anonymous
            }
        }
        Some("anonymous") => RepoCredential::Anonymous,
        Some(other) if other.starts_with("github-app:") => {
            let path = token_dir.join(id);
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    let trimmed = contents.trim();
                    if trimmed.is_empty() {
                        RepoCredential::Anonymous
                    } else {
                        RepoCredential::Header {
                            url_prefix,
                            header: format!(
                                "Authorization: Basic {}",
                                base64(format!("x-access-token:{}", trimmed).as_bytes())
                            ),
                        }
                    }
                }
                Err(_) => RepoCredential::Pending("waiting for repository credentials".to_string()),
            }
        }
        Some(literal) => RepoCredential::Header {
            url_prefix,
            header: format!(
                "Authorization: Basic {}",
                base64(format!("x-access-token:{}", literal).as_bytes())
            ),
        },
    }
}

pub fn token_dir() -> PathBuf {
    std::env::var("AIS_GIT_TOKEN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/opt/artisan/etc/git_tokens"))
}

#[cfg(test)]
#[allow(unused_imports)]
mod derived_tc_node_git_001_01 {
    use super::*;
    // case TC_NODE_GIT_001_01 begin
    #[test]
    fn case_TC_NODE_GIT_001_01() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(None, "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "https://github.com/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z3Q=".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_01 end

    // case TC_NODE_GIT_001_02 begin
    #[test]
    fn case_TC_NODE_GIT_001_02() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(None, "ab12cd34", "https://github.com/org/sub/repo.git", dir, Some("gt"));
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "https://github.com/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z3Q=".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_02 end

    // case TC_NODE_GIT_001_03 begin
    #[test]
    fn case_TC_NODE_GIT_001_03() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(None, "ab12cd34", "https://git.example.com/o/r.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_03 end

    // case TC_NODE_GIT_001_04 begin
    #[test]
    fn case_TC_NODE_GIT_001_04() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(None, "ab12cd34", "https://gitlab.com/group/project.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_04 end

    // case TC_NODE_GIT_001_05 begin
    #[test]
    fn case_TC_NODE_GIT_001_05() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(None, "ab12cd34", "https://github.com/o/r.git", dir, None);
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_05 end

    // case TC_NODE_GIT_001_07 begin
    #[test]
    fn case_TC_NODE_GIT_001_07() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("anonymous"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_07 end

    // case TC_NODE_GIT_001_08 begin
    #[test]
    fn case_TC_NODE_GIT_001_08() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("anonymous"), "ab12cd34", "https://git.custom.org/user/repo.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_08 end

    // case TC_NODE_GIT_001_09 begin
    #[test]
    fn case_TC_NODE_GIT_001_09() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("anonymous"), "ab12cd34", "http://gitea.local/repo.git", dir, None);
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_09 end

    // case TC_NODE_GIT_001_10 begin
    #[test]
    fn case_TC_NODE_GIT_001_10() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("ab12cd34"), "ghs_abc\n").unwrap();
        let res = resolve_repo_credential(Some("github-app:42"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "https://github.com/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z2hzX2FiYw==".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_10 end

    // case TC_NODE_GIT_001_11 begin
    #[test]
    fn case_TC_NODE_GIT_001_11() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("ab12cd34"), "  ghs_abc  \r\n").unwrap();
        let res = resolve_repo_credential(Some("github-app:999"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "https://github.com/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z2hzX2FiYw==".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_11 end

    // case TC_NODE_GIT_001_12 begin
    #[test]
    fn case_TC_NODE_GIT_001_12() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("ab12cd34"), "").unwrap();
        let res = resolve_repo_credential(Some("github-app:42"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_12 end

    // case TC_NODE_GIT_001_13 begin
    #[test]
    fn case_TC_NODE_GIT_001_13() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("ab12cd34"), "\n").unwrap();
        let res = resolve_repo_credential(Some("github-app:42"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_13 end

    // case TC_NODE_GIT_001_14 begin
    #[test]
    fn case_TC_NODE_GIT_001_14() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("github-app:42"), "ab12cd34", "https://github.com/o/r.git", dir, Some("gt"));
        assert_eq!(res, RepoCredential::Pending("waiting for repository credentials".to_string()));
    }
    // case TC_NODE_GIT_001_14 end

    // case TC_NODE_GIT_001_15 begin
    #[test]
    fn case_TC_NODE_GIT_001_15() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("github-app:100"), "app_missing", "https://github.com/o/r.git", dir, None);
        assert_eq!(res, RepoCredential::Pending("waiting for repository credentials".to_string()));
    }
    // case TC_NODE_GIT_001_15 end

    // case TC_NODE_GIT_001_16 begin
    #[test]
    fn case_TC_NODE_GIT_001_16() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("pat1"), "ab12cd34", "http://gitea.local/o/r.git", dir, Some("gt"));
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "http://gitea.local/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46cGF0MQ==".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_16 end

    // case TC_NODE_GIT_001_17 begin
    #[test]
    fn case_TC_NODE_GIT_001_17() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let res = resolve_repo_credential(Some("my_pat"), "ab12cd34", "https://git.internal:8443/group/repo.git", dir, None);
        assert_eq!(
            res,
            RepoCredential::Header {
                url_prefix: "https://git.internal:8443/".to_string(),
                header: "Authorization: Basic eC1hY2Nlc3MtdG9rZW46bXlfcGF0".to_string(),
            }
        );
    }
    // case TC_NODE_GIT_001_17 end

    // case TC_NODE_GIT_001_20 begin
    #[test]
    fn case_TC_NODE_GIT_001_20() {
        unsafe {
            std::env::set_var("AIS_GIT_TOKEN_DIR", "/custom/token/dir");
        }
        let custom = token_dir();
        unsafe {
            std::env::remove_var("AIS_GIT_TOKEN_DIR");
        }
        let default_dir = token_dir();
        assert_eq!(custom, std::path::PathBuf::from("/custom/token/dir"));
        assert_eq!(default_dir, std::path::PathBuf::from("/opt/artisan/etc/git_tokens"));
    }
    // case TC_NODE_GIT_001_20 end

    // case TC_NODE_GIT_001_21 begin
    #[test]
    fn case_TC_NODE_GIT_001_21() {
        let credential = RepoCredential::Header {
            url_prefix: "https://github.com/".to_string(),
            header: "Authorization: Basic test".to_string(),
        };
        let cloned = credential.clone();
        assert_eq!(credential, cloned);
    }
    // case TC_NODE_GIT_001_21 end

    // case TC_NODE_GIT_001_22 begin
    #[test]
    fn case_TC_NODE_GIT_001_22() {
        let anon = RepoCredential::Anonymous;
        let pending = RepoCredential::Pending("test".to_string());
        assert_eq!(anon, anon.clone());
        assert_eq!(pending, pending.clone());
        assert!(!format!("{:?}", pending).is_empty());
    }
    // case TC_NODE_GIT_001_22 end

    // case TC_NODE_GIT_001_23 begin
    #[test]
    fn case_TC_NODE_GIT_001_23() {
        let h1 = RepoCredential::Header {
            url_prefix: "https://github.com/".to_string(),
            header: "h1".to_string(),
        };
        let h2 = RepoCredential::Header {
            url_prefix: "https://github.com/".to_string(),
            header: "h2".to_string(),
        };
        assert_ne!(h1, h2);
        assert_ne!(h1, RepoCredential::Anonymous);
    }
    // case TC_NODE_GIT_001_23 end
}

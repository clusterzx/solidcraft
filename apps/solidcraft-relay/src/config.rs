//! The relay's settings, from its environment (docs/web-mcp.md).

use std::net::SocketAddr;
use std::path::PathBuf;

use solidcraft_mcp::{HeadlessMode, TabWaits};

/// Where the relay listens unless `SOLIDCRAFT_RELAY_LISTEN` says: this computer only.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

/// The shortest token the relay accepts.
pub const MIN_TOKEN: usize = 16;

pub struct Config {
    /// What agents send as `Authorization: Bearer …` and the tab as its first message.
    pub token: String,
    pub listen: SocketAddr,
    /// Origins (`https://cad.example`) whose pages may connect a tab, besides the relay's own.
    pub allowed_origins: Vec<String>,
    /// The web app's address, for "open it" hints; `None`: worked out from each request.
    pub public_url: Option<String>,
    pub headless: HeadlessMode,
    /// The headless session's folder on the server (its files go nowhere else).
    pub data: PathBuf,
    pub waits: TabWaits,
}

/// An origin as compared: lower case, no trailing slash.
pub fn normal_origin(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_ascii_lowercase()
}

impl Config {
    /// The settings from `var` (the environment, or a test's map). An error says what to fix; the
    /// token never appears in it.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let token = match (var("SOLIDCRAFT_RELAY_TOKEN"), var("SOLIDCRAFT_RELAY_TOKEN_FILE")) {
            (Some(t), _) if !t.is_empty() => t,
            (_, Some(file)) if !file.is_empty() => {
                std::fs::read_to_string(&file).map_err(|e| format!("SOLIDCRAFT_RELAY_TOKEN_FILE {file}: {e}"))?.trim().to_string()
            }
            _ => {
                return Err("SOLIDCRAFT_RELAY_TOKEN is not set: the relay does not start without a token. Make one with \
                            `openssl rand -hex 32` and give it to the relay (SOLIDCRAFT_RELAY_TOKEN or SOLIDCRAFT_RELAY_TOKEN_FILE), \
                            the agent (Authorization: Bearer …) and the tab (open the app with #mcp=…)"
                    .into());
            }
        };
        if token.chars().count() < MIN_TOKEN {
            return Err(format!("the relay's token is too short: give at least {MIN_TOKEN} characters (`openssl rand -hex 32` makes 64)"));
        }
        if !token.chars().all(|c| c.is_ascii_graphic()) {
            return Err("the relay's token may only hold printable ASCII characters without spaces (`openssl rand -hex 32` makes one)".into());
        }
        let listen = var("SOLIDCRAFT_RELAY_LISTEN").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_LISTEN.into());
        let listen: SocketAddr = listen
            .trim()
            .parse()
            .map_err(|e| format!("SOLIDCRAFT_RELAY_LISTEN `{listen}`: {e} (give an address and port, such as {DEFAULT_LISTEN})"))?;
        let allowed_origins = var("SOLIDCRAFT_RELAY_ALLOWED_ORIGIN")
            .unwrap_or_default()
            .split([',', ' '])
            .filter(|o| !o.trim().is_empty())
            .map(|o| {
                let o = normal_origin(o);
                if o.starts_with("http://") || o.starts_with("https://") {
                    Ok(o)
                } else {
                    Err(format!("SOLIDCRAFT_RELAY_ALLOWED_ORIGIN: `{o}` is not an origin such as https://cad.example"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let public_url = var("SOLIDCRAFT_RELAY_PUBLIC_URL").filter(|u| !u.trim().is_empty()).map(|u| {
            let u = u.trim().to_string();
            if u.ends_with('/') { u } else { format!("{u}/") }
        });
        let headless = var("SOLIDCRAFT_RELAY_HEADLESS").unwrap_or_default();
        let headless = HeadlessMode::parse(&headless).ok_or_else(|| format!("SOLIDCRAFT_RELAY_HEADLESS `{headless}`: give off, fallback or only"))?;
        let data = PathBuf::from(var("SOLIDCRAFT_RELAY_DATA").filter(|d| !d.trim().is_empty()).unwrap_or_else(|| "solidcraft-relay-data".into()));
        Ok(Self { token, listen, allowed_origins, public_url, headless, data, waits: TabWaits::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config, String> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_env(|k| vars.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone()))
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_token_is_required_and_never_shown() {
        let e = config(&[]).err().unwrap();
        assert!(e.contains("SOLIDCRAFT_RELAY_TOKEN is not set"), "{e}");
        let e = config(&[("SOLIDCRAFT_RELAY_TOKEN", "short-secret")]).err().unwrap();
        assert!(e.contains("too short") && !e.contains("short-secret"), "{e}");
        let e = config(&[("SOLIDCRAFT_RELAY_TOKEN", "with spaces in the secret token")]).err().unwrap();
        assert!(e.contains("printable") && !e.contains("secret token"), "{e}");
        let c = config(&[("SOLIDCRAFT_RELAY_TOKEN", TOKEN)]).unwrap();
        assert_eq!((c.token.as_str(), c.listen.to_string().as_str(), c.headless), (TOKEN, DEFAULT_LISTEN, HeadlessMode::Off));
    }

    #[test]
    fn the_token_can_come_from_a_file() {
        let path = std::env::temp_dir().join(format!("solidcraft-relay-token-{}", std::process::id()));
        std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
        let c = config(&[("SOLIDCRAFT_RELAY_TOKEN_FILE", &path.to_string_lossy())]).unwrap();
        assert_eq!(c.token, TOKEN);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_are_read_and_checked() {
        let c = config(&[
            ("SOLIDCRAFT_RELAY_TOKEN", TOKEN),
            ("SOLIDCRAFT_RELAY_LISTEN", "0.0.0.0:9000"),
            ("SOLIDCRAFT_RELAY_ALLOWED_ORIGIN", "https://CAD.example/, http://10.0.0.5:8080"),
            ("SOLIDCRAFT_RELAY_PUBLIC_URL", "https://cad.example/app"),
            ("SOLIDCRAFT_RELAY_HEADLESS", "fallback"),
            ("SOLIDCRAFT_RELAY_DATA", "/data"),
        ])
        .unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(c.allowed_origins, ["https://cad.example", "http://10.0.0.5:8080"]);
        assert_eq!(c.public_url.as_deref(), Some("https://cad.example/app/"));
        assert_eq!((c.headless, c.data), (HeadlessMode::Fallback, PathBuf::from("/data")));
        for (k, v) in
            [("SOLIDCRAFT_RELAY_LISTEN", "everywhere"), ("SOLIDCRAFT_RELAY_ALLOWED_ORIGIN", "cad.example"), ("SOLIDCRAFT_RELAY_HEADLESS", "maybe")]
        {
            let e = config(&[("SOLIDCRAFT_RELAY_TOKEN", TOKEN), (k, v)]).err().unwrap();
            assert!(e.starts_with(k), "{e}");
        }
    }
}

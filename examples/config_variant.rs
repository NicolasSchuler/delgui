fn main() {
    let greeting = "hello";
    println!("{} world", greeting);
}

struct Config { name: String, retries: u32, backoff: Duration, timeout: Duration }

impl Config {
    fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(Error::Parse)?;
        cfg.validate()?;
        Ok(cfg)
    }
}

// appended by watch test

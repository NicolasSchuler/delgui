fn main() {
    let greeting = "hi";
    println!("{}", greeting);
}

struct Config { name: String, retries: u32 }

impl Config {
    fn load(path: &str) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(Error::Parse)
    }
}

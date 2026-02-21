pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

pub fn random_string() -> String {
    let bytes: Vec<u8> = (0..64).map(|_| rand::random()).collect();
    hex::encode(bytes)
}

pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

pub fn random_string() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..64).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(bytes)
}

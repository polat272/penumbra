/// The key used to encrypt ciphertexts (the public key of the encryption
/// scheme).
pub struct EncryptionKey(pub decaf377::Element);

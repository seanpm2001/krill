use rpki::crypto::{
    signer::{KeyError, SigningAlgorithm},
    KeyIdentifier, PublicKey, PublicKeyFormat, RpkiSignature, Signature, SignatureAlgorithm, Signer, SigningError,
};

use crate::commons::crypto::{
    signers::{error::SignerError, softsigner::OpenSslSigner},
    SignerHandle,
};

#[cfg(all(test, feature = "hsm"))]
use crate::commons::crypto::signers::mocksigner::MockSigner;

#[cfg(feature = "hsm")]
use crate::commons::crypto::signers::{kmip::KmipSigner, pkcs11::Pkcs11Signer};

//------------ SignerProvider ------------------------------------------------

#[derive(Debug)]
pub struct SignerFlags {
    pub is_default_signer: bool,
    pub is_one_off_signer: bool,
}

impl std::fmt::Display for SignerFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "default: {}, one_off: {}",
            self.is_default_signer, self.is_one_off_signer
        ))
    }
}

impl Default for SignerFlags {
    fn default() -> Self {
        // Making round trips to an HSM to create, sign with and destroy one-off signing keys can be slow, and doesn't
        // benefit from the protection afforded by storing keys in the HSM. Therefore by default we don't use the
        // default signer for one-off signing but instead expect an OpenSSL signer to be created for this purpose.
        Self {
            is_default_signer: true,
            is_one_off_signer: false,
        }
    }
}

impl SignerFlags {
    pub fn new(is_default_signer: bool, is_one_off_signer: bool) -> Self {
        Self {
            is_default_signer,
            is_one_off_signer,
        }
    }
}

/// Dispatchers Signer requests to a particular implementation of the Signer trait.
///
/// Named and modelled after the similar AuthProvider concept that already exists in Krill.
#[allow(dead_code)] // Needed as we currently only ever construct one variant
#[derive(Debug)]
pub enum SignerProvider {
    OpenSsl(SignerFlags, OpenSslSigner),

    #[cfg(feature = "hsm")]
    Kmip(SignerFlags, KmipSigner),

    #[cfg(feature = "hsm")]
    Pkcs11(SignerFlags, Pkcs11Signer),

    #[cfg(all(test, feature = "hsm"))]
    Mock(SignerFlags, MockSigner),
}

impl SignerProvider {
    pub fn is_default_signer(&self) -> bool {
        match self {
            SignerProvider::OpenSsl(flags, _) => flags.is_default_signer,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(flags, _) => flags.is_default_signer,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(flags, _) => flags.is_default_signer,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(flags, _) => flags.is_default_signer,
        }
    }

    pub fn is_one_off_signer(&self) -> bool {
        match self {
            SignerProvider::OpenSsl(flags, _) => flags.is_one_off_signer,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(flags, _) => flags.is_one_off_signer,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(flags, _) => flags.is_one_off_signer,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(flags, _) => flags.is_one_off_signer,
        }
    }

    pub async fn create_registration_key(&self) -> Result<(PublicKey, String), SignerError> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.create_registration_key().await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.create_registration_key(),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.create_registration_key(),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.create_registration_key(),
        }
    }

    pub async fn sign_registration_challenge<D: AsRef<[u8]> + ?Sized>(
        &self,
        signer_private_key_id: &str,
        challenge: &D,
    ) -> Result<RpkiSignature, SignerError> {
        match self {
            SignerProvider::OpenSsl(_, signer) => {
                signer
                    .sign_registration_challenge(signer_private_key_id, challenge)
                    .await
            }
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.sign_registration_challenge(signer_private_key_id, challenge),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.sign_registration_challenge(signer_private_key_id, challenge),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.sign_registration_challenge(signer_private_key_id, challenge),
        }
    }

    pub fn set_handle(&self, handle: SignerHandle) {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.set_handle(handle),
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.set_handle(handle),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.set_handle(handle),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.set_handle(handle),
        }
    }

    pub fn get_name(&self) -> &str {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.get_name(),
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.get_name(),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.get_name(),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.get_name(),
        }
    }

    pub fn get_info(&self) -> Option<String> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.get_info(),
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.get_info(),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.get_info(),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.get_info(),
        }
    }

    #[cfg(all(test, feature = "hsm"))]
    pub fn wipe_all_keys(&self) {
        if let SignerProvider::Mock(_, signer) = self {
            signer.wipe_all_keys()
        }
    }

    /// Import an existing private key. Only supported for OpenSslSigner. Other
    /// signers will return an error.
    pub async fn import_key(&self, pem: &str) -> Result<KeyIdentifier, SignerError> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.import_key(pem).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, _) => Err(SignerError::other("import key not supported for KMIP signers")),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, _) => Err(SignerError::other("import key not supported for PKCS#11 signers")),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, _) => Err(SignerError::other("import key not supported for the mock signer")),
        }
    }
}

// Implement the functions defined by the `Signer` trait because `SignerRouter` expects to invoke them, but as the
// dispatching is not trait based we don't actually have to implement the `Signer` trait.
#[async_trait::async_trait]
impl Signer for SignerProvider {
    type KeyId = KeyIdentifier;
    type Error = SignerError;

    async fn create_key(&self, algorithm: PublicKeyFormat) -> Result<KeyIdentifier, SignerError> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.create_key(algorithm).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.create_key(algorithm).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.create_key(algorithm).await,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.create_key(algorithm).await,
        }
    }

    async fn get_key_info(&self, key: &KeyIdentifier) -> Result<PublicKey, KeyError<SignerError>> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.get_key_info(key).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.get_key_info(key).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.get_key_info(key).await,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.get_key_info(key).await,
        }
    }

    async fn destroy_key(&self, key: &KeyIdentifier) -> Result<(), KeyError<SignerError>> {
        match self {
            SignerProvider::OpenSsl(_, signer) => signer.destroy_key(key).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.destroy_key(key).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.destroy_key(key).await,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.destroy_key(key).await,
        }
    }

    async fn sign<Alg: SignatureAlgorithm, D: AsRef<[u8]> + ?Sized + Sync>(
        &self,
        key: &KeyIdentifier,
        algorithm: Alg,
        data: &D,
    ) -> Result<Signature<Alg>, SigningError<SignerError>> {
        let signing_algorithm = algorithm.signing_algorithm();
        if !matches!(signing_algorithm, SigningAlgorithm::RsaSha256) {
            return Err(SignerError::UnsupportedSigningAlg(signing_algorithm).into());
        }

        match self {
            SignerProvider::OpenSsl(_, signer) => signer.sign(key, algorithm, data).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.sign(key, algorithm, data).await,
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.sign(key, algorithm, data).await,
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.sign(key, algorithm, data).await,
        }
    }

    async fn sign_one_off<Alg: SignatureAlgorithm, D: AsRef<[u8]> + ?Sized + Sync>(
        &self,
        algorithm: Alg,
        data: &D,
    ) -> Result<(Signature<Alg>, PublicKey), SignerError> {
        let signing_algorithm = algorithm.signing_algorithm();
        if !matches!(signing_algorithm, SigningAlgorithm::RsaSha256) {
            return Err(SignerError::UnsupportedSigningAlg(signing_algorithm));
        }

        match self {
            SignerProvider::OpenSsl(_, signer) => signer.sign_one_off(algorithm, data),
            #[cfg(feature = "hsm")]
            SignerProvider::Kmip(_, signer) => signer.sign_one_off(algorithm, data),
            #[cfg(feature = "hsm")]
            SignerProvider::Pkcs11(_, signer) => signer.sign_one_off(algorithm, data),
            #[cfg(all(test, feature = "hsm"))]
            SignerProvider::Mock(_, signer) => signer.sign_one_off(algorithm, data),
        }
    }

    async fn rand(&self, target: &mut [u8]) -> Result<(), Self::Error> {
        openssl::rand::rand_bytes(target).map_err(SignerError::OpenSslError)
    }
}

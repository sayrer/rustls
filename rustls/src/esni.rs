use crate::client::ClientConfig;
use crate::msgs::handshake::{ESNIRecord, KeyShareEntry, ServerNamePayload, ServerName, ClientEncryptedSNI, ESNIContents, Random, PaddedServerNameList, ClientESNIInner};
use crate::msgs::enums::ServerNameType;
use crate::msgs::enums::ProtocolVersion;
use crate::suites::{KeyExchange, TLS13_CIPHERSUITES, choose_ciphersuite_preferring_server};
use crate::msgs::codec::{Codec, Reader};
use crate::rand;

use std::time::{SystemTime, UNIX_EPOCH};

use ring::{digest, hkdf};
use webpki;
use crate::SupportedCipherSuite;
use crate::msgs::base::PayloadU16;
use ring::hkdf::{KeyType, Prk};
use crate::cipher::{Iv, IvLen};
use ring::aead::{UnboundKey, Algorithm};

/// Data calculated for a client session from a DNS ESNI record.
#[derive(Clone, Debug)]
pub struct ESNIHandshakeData {
    /// The selected Key Share from the DNS record
    pub peer_share: KeyShareEntry,

    /// The selected CipherSuite from the DNS record
    pub cipher_suite: &'static SupportedCipherSuite,

    /// The length to pad the ESNI to
    pub padded_length: u16,

    /// A digest of the DNS record
    pub record_digest: Vec<u8>,
}

/// Create a TLS 1.3 Config for ESNI
pub fn create_esni_config() -> ClientConfig {
    let mut config = ClientConfig::new();
    config.versions = vec![ProtocolVersion::TLSv1_3];
    config.ciphersuites = TLS13_CIPHERSUITES.to_vec();
    config.encrypt_sni = true;
    config
}

/// Creates a `ClientConfig` with defaults suitable for ESNI extension support.
/// This creates a config that supports TLS 1.3 only.
pub fn create_esni_handshake(record_bytes: &Vec<u8>) -> Option<ESNIHandshakeData> {
    println!("ESNIKeys:{} {:02x?}", record_bytes.len(), record_bytes);
    let record = ESNIRecord::read(&mut Reader::init(&record_bytes))?;

    println!("record {:?}", record);
    // Check whether the record is still valid
    let now = now()?;

    if now < record.not_before || now > record.not_after {
        return None
    }

    let peer_share = match KeyExchange::supported_groups()
        .iter()
        .flat_map(|group| {
          record.keys.iter().find(|key| { key.group == *group })
        }).nth(0)
        .cloned() {
        Some(entry) => entry,
        None => return None,
    };

    let cipher_suite= match
        choose_ciphersuite_preferring_server(record.cipher_suites.as_slice(),
                                             &TLS13_CIPHERSUITES) {
        Some(entry) => entry,
        None => return None,
    };

    let digest = digest::digest(cipher_suite.get_hash(), record_bytes);
    let bytes: Vec<u8> = Vec::from(digest.as_ref());

    Some(ESNIHandshakeData {
        peer_share,
        cipher_suite,
        padded_length: record.padded_length,
        record_digest: bytes,
    })
}

/// Compute the encrypted SNI
// TODO: this is big and messy, fix it up
pub fn compute_esni(dns_name: webpki::DNSNameRef,
                    hs_data: &ESNIHandshakeData,
                    key_share_bytes: Vec<u8>) -> Option<ClientEncryptedSNI> {
    let mut nonce = [0u8; 16];
    rand::fill_random(&mut nonce);
    let mut sni_bytes = compute_client_esni_inner(dns_name, hs_data.padded_length, nonce);
    println!("sni_bytes: {:02x?}, {}", sni_bytes, sni_bytes.len());

    println!("Client key share: {:?}", hs_data.peer_share);
    let mut peer_bytes = Vec::new();
    hs_data.peer_share.clone().encode(&mut peer_bytes);
    println!("peer_bytes: {:02x?}, {}", peer_bytes, peer_bytes.len());

    println!("dns_name: {:?}", dns_name);
    let key_exchange = match KeyExchange::start_ecdhe(hs_data.peer_share.group) {
        Some(ke) => ke,
        None => return None,
    };

    println!("group: {:?}", key_exchange.group);


    let keyex_Bytes =  key_exchange.pubkey.as_ref();
    println!("     key_exchange: {:02x?}, {}", keyex_Bytes, keyex_Bytes.len());
    let payload = &hs_data.peer_share.payload;
    println!("payload length: {:?}", payload);
    let exchange_result = key_exchange.complete(&hs_data.peer_share.payload.0)?;
    let mut result_bytes = exchange_result.pubkey.as_ref();
    println!("   Z result_bytes: {:02x?}, {}", result_bytes, result_bytes.len());

    let premaster_bytes = exchange_result.premaster_secret.as_slice();
    println!("Z premaster_bytes: {:02x?}, {}", premaster_bytes, premaster_bytes.len());


    let mut random = [0u8; 32];
    rand::fill_random(&mut random);
    let contents = ESNIContents {
        record_digest: PayloadU16::new(hs_data.record_digest.clone()),
        esni_key_share: KeyShareEntry {
            group: hs_data.peer_share.group,
            payload: PayloadU16(exchange_result.pubkey.clone().as_ref().to_vec())
        },
        client_hello_random: Random::from_slice(&random),
    };

    let mut contents_bytes = Vec::new();
    contents.encode(&mut contents_bytes);
    println!("ESNIContents encoded, {:02x?}, {}", contents_bytes, contents_bytes.len());
    let hash = esni_hash(&contents_bytes, hs_data.cipher_suite.get_hash());
    println!("   ESNIContents hash, {:02x?}, {}", hash, hash.len());


    let zx = zx(hs_data.cipher_suite.hkdf_algorithm, &exchange_result.premaster_secret);

    let key = hkdf_expand(&zx, hs_data.cipher_suite.get_aead_alg(), b"esni key", &hash);
    println!("Key {:?}", key);
    let iv: Iv = hkdf_expand(&zx, IvLen, b"esni iv", &hash);
    println!("Iv {:02x?}", iv.value());


    println!("key_share_bytes: {:02x?}, {}", key_share_bytes, key_share_bytes.len());
    let aad = ring::aead::Aad::from(key_share_bytes.to_vec());
    let aad_bytes = aad.as_ref();
    println!("AAD: {:02x?}, {}", aad_bytes, aad_bytes.len());

    match encrypt(key, iv, aad, &mut sni_bytes) {
        Some(bytes) => {
            println!("cipher: {:02x?}, {}", bytes, bytes.len());
            Some (ClientEncryptedSNI {
                suite: hs_data.cipher_suite.suite,
                key_share_entry: KeyShareEntry::new(hs_data.peer_share.group, exchange_result.pubkey.as_ref()),
                record_digest: PayloadU16(hs_data.record_digest.clone()),
                encrypted_sni: PayloadU16(bytes),
            })
        },
        _ => None
    }
}

fn compute_client_esni_inner(dns_name: webpki::DNSNameRef, length: u16, nonce: [u8; 16]) -> Vec<u8> {
    let name = ServerName {
        typ: ServerNameType::HostName,
        payload: ServerNamePayload::HostName(dns_name.into()),
    };
    let psnl = PaddedServerNameList::new(vec![name], length);

    let mut padded_bytes = Vec::new();
    psnl.encode(&mut padded_bytes);

    let client_esni_inner = ClientESNIInner {
        nonce,
        real_sni: psnl,
    };
    let mut sni_bytes = Vec::new();
    client_esni_inner.encode(&mut sni_bytes);
    sni_bytes
}

fn esni_hash(encoded_esni_contents: &Vec<u8>, algorithm: &'static ring::digest::Algorithm) -> Vec<u8> {
    let digest = digest::digest(algorithm, &encoded_esni_contents);
    digest.as_ref().to_vec()
}

fn hkdf_expand<T, L>(secret: &Prk, key_type: L, label: &[u8], context: &[u8]) -> T
    where
        T: for <'a> From<hkdf::Okm<'a, L>>,
        L: KeyType,
{
    hkdf_expand_info(secret, key_type, label, context, |okm| okm.into())
}

fn hkdf_expand_info<F, T, L>(secret: &Prk, key_type: L, label: &[u8], context: &[u8], f: F)
                             -> T
    where
        F: for<'b> FnOnce(hkdf::Okm<'b, L>) -> T,
        L: KeyType
{
    const LABEL_PREFIX: &[u8] = b"tls13 ";

    let output_len = u16::to_be_bytes(key_type.len() as u16);
    let label_len = u8::to_be_bytes((LABEL_PREFIX.len() + label.len()) as u8);
    let context_len = u8::to_be_bytes(context.len() as u8);

    let info = &[&output_len[..], &label_len[..], LABEL_PREFIX, label, &context_len[..], context];
    let okm = secret.expand(info, key_type).unwrap();

    f(okm)
}

fn zx(algorithm: ring::hkdf::Algorithm, secret: &Vec<u8>) -> Prk {
    let zeroes = [0u8; digest::MAX_OUTPUT_LEN];
    let zeroes = &zeroes[..algorithm.len()];
    let salt = hkdf::Salt::new(algorithm, &[]);
    salt.extract(secret)
}

fn encrypt(unbound: UnboundKey, iv: Iv, aad: ring::aead::Aad<Vec<u8>>, sni_bytes: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lsk = ring::aead::LessSafeKey::new(unbound);
    match lsk.seal_in_place_append_tag(ring::aead::Nonce::assume_unique_for_key(*iv.value()),
                                       aad,
                                       sni_bytes) {
        Ok(_) => Some(sni_bytes.clone()),
        _ => None
    }
}

fn now() -> Option<u64> {
    let start = SystemTime::now();
    match start.duration_since(UNIX_EPOCH)  {
        Err(_e) => None,
        Ok(since_the_epoch) => Some(since_the_epoch.as_secs())
    }
}

struct ESNILen {
    bytes: Vec<u8>
}

impl ESNILen {
    fn new(suite: &SupportedCipherSuite) -> ESNILen {
        ESNILen {
            bytes: vec![0u8; suite.enc_key_len]
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::SupportedCipherSuite;
    use super::hkdf_expand;
    use crate::cipher::{Iv, IvLen};
    use crate::esni::ESNILen;
    use crate::msgs::handshake::ESNIRecord;
    use crate::msgs::codec::{Codec, Reader};
    use webpki;

    #[test]
    fn test_compute_client_esni_inner() {
        let nonce = hex!("c0 2b f3 39 f8 95 58 ac c4 7c d1 c6 b1 ff a7 28");

        let dns_name = webpki::DNSNameRef::try_from_ascii(b"canbe.esni.defo.ie").unwrap();

        let expected = hex!("
    c0 2b f3 39 f8 95 58 ac c4 7c d1 c6 b1 ff a7 28
    00 15 00 00 12 63 61 6e 62 65 2e 65 73 6e 69 2e
    64 65 66 6f 2e 69 65 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00");

        let result = super::compute_client_esni_inner(dns_name, 260u16, nonce);
       // assert_eq!(expected.len(), result.len());

        println!("expected: {:02x?}, {}", expected.to_vec(), expected.len());
        println!("  result: {:02x?}, {}", result, result.len());


        assert!(crate::msgs::handshake::slice_eq(&expected, &result));
    }

    #[test]
    fn test_hash() {
        let esni_bytes = hex!("
        00 20 3e 06 06 98 4c 3b a9 70 3a fb a7 a1 2d 75
    29 5b 05 81 7d 75 8f 40 9b 51 00 c8 37 8e 9d 08
    7e f1 00 1d 00 20 72 d8 3a 31 da 1c cd c7 e5 89
    c1 c6 24 bd 7a 14 2d 90 de 7f 01 82 73 9d 25 14
    c2 66 e1 97 23 5b 64 c0 c4 7c 5b c8 14 a0 a4 2b
    0c 2f f4 23 51 00 10 f4 1d f4 c1 f4 3c 3e 89 c8
    fe 87 25 d1 9f 00 ");

        let expected = hex!("
            21 5b ba fe a8 9e da 35 7b 7b 55 e4 6d 01 ac c8
            94 94 b2 6e e6 55 08 0e 47 21 6a b2 3b 7d 25 f7
        ");

        let result = super::esni_hash(&esni_bytes.to_vec(), &ring::digest::SHA256);
        assert!(crate::msgs::handshake::slice_eq(&expected, &result));
    }

    #[test]
    fn test_zx() {
        let z_bytes = hex!("
            de cf 6a 8c 23 49 e1 8c db d8 48 49 7c 10 16 9a
            77 66 fb 3f f4 8b 54 f7 bd 1f 15 14 74 e1 88 1c");

        let hash = hex!("
            a5 33 9b 1b a6 ae d2 7f 43 b9 91 5e 5e bc 8e 5a
            af d9 fb 1d e2 b4 df 36 13 70 97 14 27 a1 61 25
        ");

        let expected_iv = hex!("
            07 d7 77 4c 69 be bd ad 1b 75 49 c7
        ");

        let aad_bytes = hex!("
        00 69 00 1d 00 20 70 cb
        7e ce 36 ab c1 b6 e1 92 6a 9a f2 08 d9 91 70 f1
        98 7a aa 0f e3 9b f0 b3 c5 4d 79 00 a8 07 00 17
        00 41 04 03 1d 6c 6c e6 f3 28 1f 6f f2 78 d5 5c
        0f 5e f7 be 52 71 9f 7e c0 0e 6e 26 db 85 7b f9
        e0 73 91 e6 b5 3e 06 7b ef c8 f8 b5 f0 46 16 c2
        9f 0d 52 c3 6a 9e 41 2f 68 ce 7e ee d0 27 99 e5
        28 aa 9e
        ");

        let plain_text = hex!("
        4f b0 25 11 6b f7 4d f8 ce f3 0f 59 ce d9 d6 df
    00 17 00 00 14 63 64 6e 6a 73 2e 63 6c 6f 75 64
    66 6c 61 72 65 2e 63 6f 6d 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00
        ");

        let expected = hex!("
        28 0a 3d 56 cd 30 9d 68 ac 98 1b 41 bb fb 85 26
    48 ef 1a 83 c8 aa bd 12 15 80 44 10 50 2f c0 3d
    68 15 99 e0 47 6a 80 c2 e9 a0 df 86 16 7e a8 a4
    37 8c 27 62 89 7e f8 60 4f 04 cf b5 ea 60 ed 99
    51 59 70 a1 a5 ac b9 32 7d 35 86 e9 e2 01 d6 60
    9d 8d de 81 03 69 13 dd 66 09 e9 18 76 f9 25 65
    3d b7 ea 22 50 da 50 4d d8 74 31 5a 35 a2 29 7a
    09 31 0a 45 4e b2 29 fd 72 40 04 93 3a e3 a6 7d
    09 46 bb b5 8d e0 0f b5 12 e4 36 7d 38 32 3b b5
    ee 99 6f ad 2c ea af 39 9f a1 dc c9 70 dc 2f ad
    46 de 2a d6 8c 4e 3c e6 31 01 8a 97 f0 1f c9 3c
    b8 c8 f1 45 02 c4 d7 3d ee b9 88 6f 53 cc 85 0b
    69 ce 61 dc 30 c8 85 2d e1 d0 d3 d6 10 c2 32 04
    0d 96 2d d5 4a a4 1f e2 bc a3 77 15 72 61 20 75
    aa 9b 4a ee f7 25 cf 22 95 b9 77 88 48 f3 30 8e
    a4 ab 3d b4 bd b4 e4 24 98 b7 ca 7e bf 26 ee 82
    b5 b4 fd f2 f0 65 04 ea 4c 7c 75 25 24 b0 be 92
    9a a2 b7 e4 82 5a 37 cf 08 3f 0e 9b 6c 89 27 b4
    33 15 75 24
        ");

        //let expected = hex!("4a 44 bd 36 07 73 51 95 a6 27 cc 81 c9 a5 6c fe");

        let zx = super::zx(crate::suites::TLS13_AES_128_GCM_SHA256.hkdf_algorithm, &z_bytes.to_vec());
        println!("{:?}", zx);
        let aead_alg = crate::suites::TLS13_AES_128_GCM_SHA256.get_aead_alg();
        let key :ring::aead::UnboundKey = hkdf_expand(&zx, aead_alg, b"esni key", hash.as_ref());

        let iv: Iv = hkdf_expand(&zx, IvLen, b"esni iv", hash.as_ref());
        println!("Iv {:02x?}", iv.value());
        assert!(crate::msgs::handshake::slice_eq(&expected_iv, iv.value()));

        let aad = ring::aead::Aad::from(aad_bytes.to_vec());
        let mut sni_bytes = Vec::from(plain_text.to_vec());
        let encrypted = super::encrypt(key, iv, aad, &mut sni_bytes).unwrap();
        println!("{}, {:02x?}", encrypted.len(), encrypted);

        assert!(crate::msgs::handshake::slice_eq(&expected, &encrypted));
    }

    #[test]
    fn test_encrypt() {
        let key_bytes = hex!("9b 9f f2 2c dd 39 4c f6 20 ac f8 d6 f6 90 99 ab");

        let iv_bytes = hex!("d0 c2 2c 42 3c 03 a7 1d 3d 36 36 51");

        let aad_bytes = hex!("
    00 69 00 1d 00 20 e7 41
    94 4b 78 8d 6f cd 6b 5b 64 f6 69 35 83 d1 df c7
    e8 21 55 c6 f7 8d a5 c3 25 b9 7a 69 58 7d 00 17
    00 41 04 d8 75 ac 7c 46 38 c6 eb 35 a9 90 60 6b
    1b be b1 70 dd 18 0c 80 82 8d 83 95 b1 aa a5 2e
    24 2e fb ed 9f 2a bd 7f 86 f0 8c 8b 6b ca db a6
    28 69 88 1d fb 76 5f 34 d9 da 0b 07 02 64 80 d2
    d3 84 15 ");

        let mut plain_text = hex!("
    ad 1b f4 b3 d3 14 59 48 59 9e be c8 56 42 4f 66
    00 15 00 00 12 63 61 6e 62 65 2e 65 73 6e 69 2e
    64 65 66 6f 2e 69 65 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
    00 00 00 00 ");

        let expected = hex!("
    6f f6 5d 1e bd 9c 35 2d 2c 1c ca 92 5d 3e 1a 65
    f6 30 fe 97 3b a0 24 9d 92 b8 cb 67 f0 1d 17 a4
    bc 11 9b ac 39 c4 48 f7 bb 86 04 b5 58 ad 76 15
    10 c3 21 d0 3b 86 ac c9 d6 7e 9f 89 6e b0 73 cb
    69 97 f4 1b f5 17 e9 81 29 86 6f 3e df 49 99 3c
    59 00 24 6c 2d d6 3e 7b d2 b7 bd 3a c0 90 8f b6
    dc 2b 11 08 15 00 41 ca fb 79 ef 57 5b 17 18 00
    bf c2 0c 1b 2b cf 1e 9b f0 0f 9d 67 32 37 e1 06
    22 f8 cb a8 a3 40 26 6e 50 85 32 29 d7 20 41 a5
    0f 47 87 d0 af 01 ba 83 62 ad a0 b6 ac 8e d5 dd
    24 42 3d f8 a8 f9 9e 16 40 cf 85 b9 16 39 f8 94
    4b bd cb a5 59 a8 a9 65 7a 83 95 b2 38 c7 3b d5
    d4 9b 6f f0 e3 18 d0 cb 65 65 c9 0c 8a 07 a1 ce
    5f 39 ed 6a 1b 6f e7 59 11 7d b3 81 e4 4b 51 d4
    db 28 f3 95 eb 16 62 de de 29 c7 dc 79 54 67 24
    d7 4d d1 3f 34 ca 64 6e 6c 12 9a e4 0c 1c ea 33
    c3 81 15 48 04 14 a4 ed ab 44 90 e9 0d c2 56 8a
    df 4e 92 eb 3b 93 f5 5c 59 15 0e 7d 85 66 2d b4
    62 ee 41 8a");

        let key = ring::aead::UnboundKey::new(crate::suites::TLS13_AES_128_GCM_SHA256.get_aead_alg(), &key_bytes).unwrap();
        let iv = crate::cipher::Iv::new(iv_bytes);
        let aad = ring::aead::Aad::from(aad_bytes.to_vec());
        let mut sni_bytes = Vec::from(plain_text.to_vec());
        let encrypted = super::encrypt(key, iv, aad, &mut sni_bytes).unwrap();
        println!("{}, {:02x?}", encrypted.len(), encrypted);
        assert_eq!(expected.len(), encrypted.len());
        assert!(crate::msgs::handshake::slice_eq(&expected, encrypted.as_slice()));
    }

    /// Generic newtype wrapper that lets us implement traits for externally-defined
    /// types.
    #[derive(Debug, PartialEq)]
    struct My<T: core::fmt::Debug + PartialEq>(T);

    impl ring::hkdf::KeyType for My<usize> {
        fn len(&self) -> usize {
            self.0
        }
    }

    impl From<ring::hkdf::Okm<'_, My<usize>>> for My<Vec<u8>> {
        fn from(okm: ring::hkdf::Okm<My<usize>>) -> Self {
            let mut r = vec![0u8; okm.len().0];
            okm.fill(&mut r).unwrap();
            My(r)
        }
    }
}
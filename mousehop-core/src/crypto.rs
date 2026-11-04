use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};
use thiserror::Error;
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

pub fn certificate_fingerprint(cert: &Certificate) -> String {
    let certificate = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(certificate)
}

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        Ok(load_certificate(path)?)
    } else {
        generate_key_and_cert(path)
    }
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    #[cfg(not(windows))]
    let f = File::create(path)?;
    #[cfg(windows)]
    let f = if mousehop_ipc::windows::service_mode() {
        use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
        use windows::{
            Win32::Storage::FileSystem::{
                CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_READ,
            },
            core::PCWSTR,
        };
        // The shared directory is readable by Users. Set the private key's
        // non-inheriting DACL at creation, before any key bytes are written.
        let descriptor =
            mousehop_ipc::windows::SecurityDescriptor::new("D:P(A;;FA;;;SY)(A;;FA;;;BA)")?;
        let attributes = descriptor.attributes();
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let handle = CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ,
                Some(&attributes),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(io::Error::from)?;
            File::from_raw_handle(handle.0)
        }
    } else {
        File::create(path)?
    };
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400); /* r-- --- --- */
        f.set_permissions(perm)?;
    }
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    Ok(cert)
}

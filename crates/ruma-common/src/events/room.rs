//! Modules for events in the `m.room` namespace.
//!
//! This module also contains types shared by events in its child namespaces.

use std::collections::BTreeMap;

use js_int::UInt;
use serde::{de, Deserialize, Serialize};

#[cfg(feature = "unstable-msc3551")]
use super::file::{EncryptedContent, EncryptedContentInit, FileContent};
#[cfg(feature = "unstable-msc3552")]
use super::{
    file::FileContentInfo,
    image::{ImageContent, ThumbnailContent, ThumbnailFileContent, ThumbnailFileContentInfo},
};
#[cfg(feature = "unstable-msc3551")]
use crate::MxcUri;
use crate::{
    serde::{base64::UrlSafe, Base64},
    OwnedMxcUri,
};

pub mod aliases;
pub mod avatar;
pub mod canonical_alias;
pub mod create;
pub mod encrypted;
pub mod encryption;
pub mod guest_access;
pub mod history_visibility;
pub mod join_rules;
pub mod member;
pub mod message;
pub mod name;
pub mod pinned_events;
pub mod power_levels;
pub mod redaction;
pub mod server_acl;
pub mod third_party_invite;
mod thumbnail_source_serde;
pub mod tombstone;
pub mod topic;

/// The source of a media file.
#[derive(Clone, Debug, Serialize)]
#[allow(clippy::exhaustive_enums)]
pub enum MediaSource {
    /// The MXC URI to the unencrypted media file.
    #[serde(rename = "url")]
    Plain(OwnedMxcUri),

    /// The encryption info of the encrypted media file.
    #[serde(rename = "file")]
    Encrypted(Box<EncryptedFile>),
}

#[cfg(feature = "unstable-msc3551")]
impl MediaSource {
    pub(crate) fn into_extensible_content(self) -> (OwnedMxcUri, Option<EncryptedContent>) {
        match self {
            MediaSource::Plain(url) => (url, None),
            MediaSource::Encrypted(encrypted_file) => {
                let EncryptedFile { url, key, iv, hashes, v } = *encrypted_file;
                (url, Some(EncryptedContentInit { key, iv, hashes, v }.into()))
            }
        }
    }
}

// Custom implementation of `Deserialize`, because serde doesn't guarantee what variant will be
// deserialized for "externally tagged"¹ enums where multiple "tag" fields exist.
//
// ¹ https://serde.rs/enum-representations.html
impl<'de> Deserialize<'de> for MediaSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        pub struct MediaSourceJsonRepr {
            url: Option<OwnedMxcUri>,
            file: Option<Box<EncryptedFile>>,
        }

        match MediaSourceJsonRepr::deserialize(deserializer)? {
            MediaSourceJsonRepr { url: None, file: None } => Err(de::Error::missing_field("url")),
            // Prefer file if it is set
            MediaSourceJsonRepr { file: Some(file), .. } => Ok(MediaSource::Encrypted(file)),
            MediaSourceJsonRepr { url: Some(url), .. } => Ok(MediaSource::Plain(url)),
        }
    }
}

#[cfg(feature = "unstable-msc3551")]
impl From<&FileContent> for MediaSource {
    fn from(content: &FileContent) -> Self {
        let FileContent { url, encryption_info, .. } = content;
        if let Some(encryption_info) = encryption_info.as_deref() {
            Self::Encrypted(Box::new(EncryptedFile::from_extensible_content(url, encryption_info)))
        } else {
            Self::Plain(url.to_owned())
        }
    }
}

#[cfg(feature = "unstable-msc3552")]
impl From<&ThumbnailFileContent> for MediaSource {
    fn from(content: &ThumbnailFileContent) -> Self {
        let ThumbnailFileContent { url, encryption_info, .. } = content;
        if let Some(encryption_info) = encryption_info.as_deref() {
            Self::Encrypted(Box::new(EncryptedFile::from_extensible_content(url, encryption_info)))
        } else {
            Self::Plain(url.to_owned())
        }
    }
}

/// Metadata about an image.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub struct ImageInfo {
    /// The height of the image in pixels.
    #[serde(rename = "h", skip_serializing_if = "Option::is_none")]
    pub height: Option<UInt>,

    /// The width of the image in pixels.
    #[serde(rename = "w", skip_serializing_if = "Option::is_none")]
    pub width: Option<UInt>,

    /// The MIME type of the image, e.g. "image/png."
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,

    /// The file size of the image in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<UInt>,

    /// Metadata about the image referred to in `thumbnail_source`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_info: Option<Box<ThumbnailInfo>>,

    /// The source of the thumbnail of the image.
    #[serde(flatten, with = "thumbnail_source_serde", skip_serializing_if = "Option::is_none")]
    pub thumbnail_source: Option<MediaSource>,

    /// The [BlurHash](https://blurha.sh) for this image.
    ///
    /// This uses the unstable prefix in
    /// [MSC2448](https://github.com/matrix-org/matrix-spec-proposals/pull/2448).
    #[cfg(feature = "unstable-msc2448")]
    #[serde(
        rename = "xyz.amorgan.blurhash",
        alias = "blurhash",
        skip_serializing_if = "Option::is_none"
    )]
    pub blurhash: Option<String>,
}

impl ImageInfo {
    /// Creates an empty `ImageInfo`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an `ImageInfo` from the given file info, image info and thumbnail.
    #[cfg(feature = "unstable-msc3552")]
    pub fn from_extensible_content(
        file_info: Option<&FileContentInfo>,
        image: &ImageContent,
        thumbnail: &[ThumbnailContent],
    ) -> Option<Self> {
        if file_info.is_none() && image.is_empty() && thumbnail.is_empty() {
            None
        } else {
            let (mimetype, size) = file_info
                .map(|info| (info.mimetype.to_owned(), info.size.to_owned()))
                .unwrap_or_default();
            let ImageContent { height, width } = image.to_owned();
            let (thumbnail_source, thumbnail_info) = thumbnail
                .get(0)
                .map(|thumbnail| {
                    let source = (&thumbnail.file).into();
                    let info = ThumbnailInfo::from_extensible_content(
                        thumbnail.file.info.as_deref(),
                        thumbnail.image.as_deref(),
                    )
                    .map(Box::new);
                    (Some(source), info)
                })
                .unwrap_or_default();

            Some(Self {
                height,
                width,
                mimetype,
                size,
                thumbnail_source,
                thumbnail_info,
                #[cfg(feature = "unstable-msc2448")]
                blurhash: None,
            })
        }
    }
}

/// Metadata about a thumbnail.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub struct ThumbnailInfo {
    /// The height of the thumbnail in pixels.
    #[serde(rename = "h", skip_serializing_if = "Option::is_none")]
    pub height: Option<UInt>,

    /// The width of the thumbnail in pixels.
    #[serde(rename = "w", skip_serializing_if = "Option::is_none")]
    pub width: Option<UInt>,

    /// The MIME type of the thumbnail, e.g. "image/png."
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,

    /// The file size of the thumbnail in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<UInt>,
}

impl ThumbnailInfo {
    /// Creates an empty `ThumbnailInfo`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `ThumbnailInfo` with the given file info and image info.
    ///
    /// Returns `None` if `file_info` and `image` are `None`.
    #[cfg(feature = "unstable-msc3552")]
    pub fn from_extensible_content(
        file_info: Option<&ThumbnailFileContentInfo>,
        image: Option<&ImageContent>,
    ) -> Option<Self> {
        if file_info.is_none() && image.is_none() {
            None
        } else {
            let ThumbnailFileContentInfo { mimetype, size } =
                file_info.map(ToOwned::to_owned).unwrap_or_default();
            let ImageContent { height, width } = image.map(ToOwned::to_owned).unwrap_or_default();
            Some(Self { height, width, mimetype, size })
        }
    }
}

/// A file sent to a room with end-to-end encryption enabled.
///
/// To create an instance of this type, first create a `EncryptedFileInit` and convert it via
/// `EncryptedFile::from` / `.into()`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub struct EncryptedFile {
    /// The URL to the file.
    pub url: OwnedMxcUri,

    /// A [JSON Web Key](https://tools.ietf.org/html/rfc7517#appendix-A.3) object.
    pub key: JsonWebKey,

    /// The 128-bit unique counter block used by AES-CTR, encoded as unpadded base64.
    pub iv: Base64,

    /// A map from an algorithm name to a hash of the ciphertext, encoded as unpadded base64.
    ///
    /// Clients should support the SHA-256 hash, which uses the key sha256.
    pub hashes: BTreeMap<String, Base64>,

    /// Version of the encrypted attachments protocol.
    ///
    /// Must be `v2`.
    pub v: String,
}

#[cfg(feature = "unstable-msc3551")]
impl EncryptedFile {
    /// Create an `EncryptedFile` from the given url and encryption info.
    pub fn from_extensible_content(url: &MxcUri, encryption_info: &EncryptedContent) -> Self {
        let EncryptedContent { key, iv, hashes, v } = encryption_info.to_owned();
        Self { url: url.to_owned(), key, iv, hashes, v }
    }
}

/// Initial set of fields of `EncryptedFile`.
///
/// This struct will not be updated even if additional fields are added to `EncryptedFile` in a new
/// (non-breaking) release of the Matrix specification.
#[derive(Debug)]
#[allow(clippy::exhaustive_structs)]
pub struct EncryptedFileInit {
    /// The URL to the file.
    pub url: OwnedMxcUri,

    /// A [JSON Web Key](https://tools.ietf.org/html/rfc7517#appendix-A.3) object.
    pub key: JsonWebKey,

    /// The 128-bit unique counter block used by AES-CTR, encoded as unpadded base64.
    pub iv: Base64,

    /// A map from an algorithm name to a hash of the ciphertext, encoded as unpadded base64.
    ///
    /// Clients should support the SHA-256 hash, which uses the key sha256.
    pub hashes: BTreeMap<String, Base64>,

    /// Version of the encrypted attachments protocol.
    ///
    /// Must be `v2`.
    pub v: String,
}

impl From<EncryptedFileInit> for EncryptedFile {
    fn from(init: EncryptedFileInit) -> Self {
        let EncryptedFileInit { url, key, iv, hashes, v } = init;
        Self { url, key, iv, hashes, v }
    }
}

/// A [JSON Web Key](https://tools.ietf.org/html/rfc7517#appendix-A.3) object.
///
/// To create an instance of this type, first create a `JsonWebKeyInit` and convert it via
/// `JsonWebKey::from` / `.into()`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(not(feature = "unstable-exhaustive-types"), non_exhaustive)]
pub struct JsonWebKey {
    /// Key type.
    ///
    /// Must be `oct`.
    pub kty: String,

    /// Key operations.
    ///
    /// Must at least contain `encrypt` and `decrypt`.
    pub key_ops: Vec<String>,

    /// Algorithm.
    ///
    /// Must be `A256CTR`.
    pub alg: String,

    /// The key, encoded as url-safe unpadded base64.
    pub k: Base64<UrlSafe>,

    /// Extractable.
    ///
    /// Must be `true`. This is a
    /// [W3C extension](https://w3c.github.io/webcrypto/#iana-section-jwk).
    pub ext: bool,
}

/// Initial set of fields of `JsonWebKey`.
///
/// This struct will not be updated even if additional fields are added to `JsonWebKey` in a new
/// (non-breaking) release of the Matrix specification.
#[derive(Debug)]
#[allow(clippy::exhaustive_structs)]
pub struct JsonWebKeyInit {
    /// Key type.
    ///
    /// Must be `oct`.
    pub kty: String,

    /// Key operations.
    ///
    /// Must at least contain `encrypt` and `decrypt`.
    pub key_ops: Vec<String>,

    /// Algorithm.
    ///
    /// Must be `A256CTR`.
    pub alg: String,

    /// The key, encoded as url-safe unpadded base64.
    pub k: Base64<UrlSafe>,

    /// Extractable.
    ///
    /// Must be `true`. This is a
    /// [W3C extension](https://w3c.github.io/webcrypto/#iana-section-jwk).
    pub ext: bool,
}

impl From<JsonWebKeyInit> for JsonWebKey {
    fn from(init: JsonWebKeyInit) -> Self {
        let JsonWebKeyInit { kty, key_ops, alg, k, ext } = init;
        Self { kty, key_ops, alg, k, ext }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use serde::Deserialize;
    use serde_json::{from_value as from_json_value, json};

    use crate::{mxc_uri, serde::Base64};

    use super::{EncryptedFile, JsonWebKey, MediaSource};

    #[derive(Deserialize)]
    struct MsgWithAttachment {
        #[allow(dead_code)]
        body: String,
        #[serde(flatten)]
        source: MediaSource,
    }

    fn dummy_jwt() -> JsonWebKey {
        JsonWebKey {
            kty: "oct".to_owned(),
            key_ops: vec!["encrypt".to_owned(), "decrypt".to_owned()],
            alg: "A256CTR".to_owned(),
            k: Base64::new(vec![0; 64]),
            ext: true,
        }
    }

    fn encrypted_file() -> EncryptedFile {
        EncryptedFile {
            url: mxc_uri!("mxc://localhost/encryptedfile").to_owned(),
            key: dummy_jwt(),
            iv: Base64::new(vec![0; 64]),
            hashes: BTreeMap::new(),
            v: "v2".to_owned(),
        }
    }

    #[test]
    fn prefer_encrypted_attachment_over_plain() {
        let msg: MsgWithAttachment = from_json_value(json!({
            "body": "",
            "url": "mxc://localhost/file",
            "file": encrypted_file(),
        }))
        .unwrap();

        assert_matches!(msg.source, MediaSource::Encrypted(_));

        // As above, but with the file field before the url field
        let msg: MsgWithAttachment = from_json_value(json!({
            "body": "",
            "file": encrypted_file(),
            "url": "mxc://localhost/file",
        }))
        .unwrap();

        assert_matches!(msg.source, MediaSource::Encrypted(_));
    }
}

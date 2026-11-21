//! Native media projection. Binary inline payloads are encoded incrementally; their
//! protobuf size does not determine the size of a temporary allocation.
use super::wire::Message;
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::{Read, Write};

/// Returns whether any item names a local file that is gone and carries no bytes of its own.
/// Combines `Media` (`media_fields`: `1: mime_type, 2: inline_data, 4: description, 5: uri, 9: display_name`)
/// and legacy `ImageData` (`image_fields`: `1: base64_data, 2: mime_type, 3: caption, 4: uri`) into one
/// `"media":[...]` array without whole-image heap allocations.
pub(super) fn project(
    file: &File,
    parent: &Message,
    media_fields: &[u32],
    image_fields: &[u32],
    out: &mut impl Write,
) -> Result<bool> {
    let mut missing = false;
    let mut first = true;
    out.write_all(b",\"media\":[")?;
    for &number in media_fields {
        for field in parent.repeated(number) {
            if !std::mem::take(&mut first) {
                out.write_all(b",")?;
            }
            let item = field.message(file)?;
            missing |= project_media_item(file, &item, out)?;
        }
    }
    for &number in image_fields {
        for field in parent.repeated(number) {
            if !std::mem::take(&mut first) {
                out.write_all(b",")?;
            }
            let item = field.message(file)?;
            missing |= project_image_item(file, &item, out)?;
        }
    }
    out.write_all(b"]")?;
    Ok(missing)
}

fn nonempty(field: super::wire::Field) -> bool {
    matches!(field.value, super::wire::Value::Bytes { len, .. } if len > 0)
}

fn project_media_item(file: &File, item: &Message, out: &mut impl Write) -> Result<bool> {
    let mut missing = false;
    out.write_all(b"{\"native\":true")?;
    super::text(file, item, 1, "mime_type", out)?;
    let link = item.last(5).filter(|field| nonempty(*field));
    // A prompt image is stored twice: inline, and as a file beside the exported
    // logs. The file is the cheap reference while it exists; the bytes outlive it.
    let gone = match link {
        Some(link) => local_file_missing(file, link)?,
        None => false,
    };
    let inline = item.last(2);
    if link.is_some() && !(gone && inline.is_some()) {
        super::text(file, item, 5, "uri", out)?;
        missing |= gone;
    } else if let Some(data) = inline {
        let mime = inline_mime(file, item, 1)?;
        write!(out, ",\"uri\":\"data:{mime};base64,")?;
        base64(data.reader(file)?, out)?;
        out.write_all(b"\"")?;
    }
    super::text(file, item, 4, "description", out)?;
    super::text(file, item, 9, "display_name", out)?;
    // A blobref has not been resolved here. Its original bytes remain in the
    // COMPLETE wire record, with the adapter's unsupported-field coverage.
    out.write_all(b"}")?;
    Ok(missing)
}

fn project_image_item(file: &File, item: &Message, out: &mut impl Write) -> Result<bool> {
    let mut missing = false;
    out.write_all(b"{\"native\":true")?;
    super::text(file, item, 2, "mime_type", out)?;
    let link = item.last(4).filter(|field| nonempty(*field));
    let gone = match link {
        Some(link) => local_file_missing(file, link)?,
        None => false,
    };
    let inline = item.last(1);
    let has_inline_bytes = inline.is_some_and(nonempty);
    if link.is_some() && !(gone && has_inline_bytes) {
        super::text(file, item, 4, "uri", out)?;
        missing |= gone;
    } else if let Some(data) = inline {
        let mime = inline_mime(file, item, 2)?;
        write!(out, ",\"uri\":\"data:{mime};base64,")?;
        base64_str(data.reader(file)?, out)?;
        out.write_all(b"\"")?;
    }
    super::text(file, item, 3, "description", out)?;
    out.write_all(b"}")?;
    Ok(missing)
}

/// Only a path or `file:` URI can be checked; any other reference is not known missing.
fn local_file_missing(file: &File, link: super::wire::Field) -> Result<bool> {
    let mut uri = String::new();
    if link.reader(file)?.take(4097).read_to_string(&mut uri).is_err() || uri.len() > 4096 {
        return Ok(false);
    }
    Ok(crate::harness::antigravity::attachment_missing(&uri))
}

fn inline_mime(file: &File, item: &Message, mime_field: u32) -> Result<String> {
    let Some(field) = item.last(mime_field) else {
        return Ok("application/octet-stream".into());
    };
    let mut mime = String::new();
    field
        .reader(file)?
        .take(257)
        .read_to_string(&mut mime)
        .context("invalid native media MIME UTF-8")?;
    ensure!(mime.len() <= 256, "native media MIME exceeds 256 bytes");
    if mime.is_empty() {
        return Ok("application/octet-stream".into());
    }
    let essence = mime.split(';').next().unwrap_or_default();
    let valid_type = essence
        .split_once('/')
        .is_some_and(|(top, sub)| !top.is_empty() && !sub.is_empty() && !sub.contains('/'));
    ensure!(
        valid_type
            && mime
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-/;=".contains(&b)),
        "invalid native media MIME for data URI"
    );
    Ok(mime)
}

/// Stream an already-base64-encoded UTF-8 payload (`ImageData.base64_data`) in fixed stack
/// chunks, stripping ASCII whitespace and rejecting bytes outside `A-Za-z0-9+/=` or malformed
/// padding without allocating the whole string.
fn base64_str(mut input: impl Read, out: &mut impl Write) -> Result<()> {
    let mut raw = [0u8; 8192];
    let mut clean = [0u8; 8192];
    let mut total = 0u64;
    let mut padding = 0u8;
    loop {
        let n = match input.read(&mut raw) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let mut written = 0;
        for &b in &raw[..n] {
            match b {
                b' ' | b'\t' | b'\r' | b'\n' => continue,
                b'=' => {
                    padding += 1;
                    ensure!(padding <= 2, "invalid native ImageData base64 padding");
                    clean[written] = b;
                    written += 1;
                    total += 1;
                }
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => {
                    ensure!(padding == 0, "invalid native ImageData base64 data after padding");
                    clean[written] = b;
                    written += 1;
                    total += 1;
                }
                _ => anyhow::bail!("invalid native ImageData base64 character"),
            }
        }
        if written > 0 {
            out.write_all(&clean[..written])?;
        }
    }
    ensure!(total.is_multiple_of(4), "invalid native ImageData base64 length");
    Ok(())
}

pub(super) fn base64(mut input: impl Read, out: &mut impl Write) -> Result<()> {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = [0u8; 6144];
    let mut encoded = [0u8; 8192];
    loop {
        let mut n = 0;
        while n < bytes.len() {
            match input.read(&mut bytes[n..]) {
                Ok(0) => break,
                Ok(read) => n += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if n == 0 {
            return Ok(());
        }
        let mut written = 0;
        for chunk in bytes[..n].chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            encoded[written] = DIGITS[(a >> 2) as usize];
            encoded[written + 1] = DIGITS[(((a & 3) << 4) | (b >> 4)) as usize];
            encoded[written + 2] = if chunk.len() > 1 {
                DIGITS[(((b & 15) << 2) | (c >> 6)) as usize]
            } else {
                b'='
            };
            encoded[written + 3] = if chunk.len() > 2 {
                DIGITS[(c & 63) as usize]
            } else {
                b'='
            };
            written += 4;
        }
        out.write_all(&encoded[..written])?;
        if n < bytes.len() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(field: u64, value: &[u8]) -> Vec<u8> {
        fn varint(mut value: u64, out: &mut Vec<u8>) {
            loop {
                let b = (value & 127) as u8;
                value >>= 7;
                out.push(b | if value == 0 { 0 } else { 128 });
                if value == 0 {
                    return;
                }
            }
        }
        let mut out = Vec::new();
        varint((field << 3) | 2, &mut out);
        varint(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }
    fn projected(item: &[u8]) -> Result<serde_json::Value> {
        let bytes = bytes(9, item);
        let source = crate::harness::store::bytes::ByteSource::arena()?;
        source.append(|out| {
            out.write_all(&bytes)?;
            Ok(())
        })?;
        let file = source.file()?;
        let parent = Message::read(&file, 0, bytes.len() as u64)?;
        let mut out = b"{\"test\":true".to_vec();
        project(&file, &parent, &[9], &[], &mut out)?;
        out.push(b'}');
        Ok(serde_json::from_slice(&out)?)
    }
    fn projected_combined(
        parent_bytes: &[u8],
        media_fields: &[u32],
        image_fields: &[u32],
    ) -> Result<(bool, serde_json::Value)> {
        let source = crate::harness::store::bytes::ByteSource::arena()?;
        source.append(|out| {
            out.write_all(parent_bytes)?;
            Ok(())
        })?;
        let file = source.file()?;
        let parent = Message::read(&file, 0, parent_bytes.len() as u64)?;
        let mut out = b"{\"test\":true".to_vec();
        let missing = project(&file, &parent, media_fields, image_fields, &mut out)?;
        out.push(b'}');
        Ok((missing, serde_json::from_slice(&out)?))
    }
    #[test]
    fn binary_inline_media_and_padding_are_preserved() {
        for (input, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&[0, 255, 128, 1][..], "AP+AAQ=="),
        ] {
            let item = [bytes(1, b"image/png"), bytes(2, input)].concat();
            assert_eq!(
                projected(&item).unwrap()["media"][0]["uri"],
                format!("data:image/png;base64,{expected}")
            );
        }
    }
    #[test]
    fn existing_uri_takes_precedence_and_invalid_mime_is_rejected() {
        // A linked file that exists is the reference; one that is gone yields to the bytes.
        let existing = std::env::current_exe().unwrap();
        let existing = existing.to_str().unwrap();
        let item = [
            bytes(1, b"image/png"),
            bytes(2, b"inline"),
            bytes(5, existing.as_bytes()),
        ]
        .concat();
        assert_eq!(projected(&item).unwrap()["media"][0]["uri"], existing);
        let gone = [
            bytes(1, b"image/png"),
            bytes(2, b"inline"),
            bytes(5, b"file:///nonexistent/cv-fixture/image.png"),
        ]
        .concat();
        assert_eq!(
            projected(&gone).unwrap()["media"][0]["uri"],
            "data:image/png;base64,aW5saW5l"
        );
        let empty_uri = [bytes(1, b"image/png"), bytes(2, b"x"), bytes(5, b"")].concat();
        assert_eq!(
            projected(&empty_uri).unwrap()["media"][0]["uri"],
            "data:image/png;base64,eA=="
        );
        assert!(projected(&[bytes(1, b"image/png,evil\""), bytes(2, b"x")].concat()).is_err());
        assert!(projected(&[bytes(1, &vec![b'a'; 257]), bytes(2, b"x")].concat()).is_err());
    }
    #[test]
    fn encoding_crosses_buffer_boundaries_without_losing_remainders() {
        let mut data = vec![b'f'; 6144];
        data.extend_from_slice(b"fo");
        let mut encoded = Vec::new();
        base64(data.as_slice(), &mut encoded).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            format!("{}Zm8=", "ZmZm".repeat(2048))
        );
    }

    #[test]
    fn legacy_image_data_combines_with_media_prefers_existing_uri_and_validates_base64() {
        let existing = std::env::current_exe().unwrap();
        let existing = existing.to_str().unwrap();

        let media_item = [bytes(1, b"image/png"), bytes(2, b"foo")].concat();
        let image_inline = [
            bytes(1, b"  aGVs\r\nbG8= \n"),
            bytes(2, b"image/jpeg"),
            bytes(3, b"legacy screenshot"),
        ]
        .concat();
        let image_existing_uri = [
            bytes(1, b"aGVsbG8="),
            bytes(2, b"image/webp"),
            bytes(4, existing.as_bytes()),
        ]
        .concat();
        let image_gone_with_inline = [
            bytes(1, b"aGVsbG8="),
            bytes(2, b"image/gif"),
            bytes(4, b"file:///nonexistent/cv-fixture/legacy.gif"),
        ]
        .concat();
        let parent = [
            bytes(9, &media_item),
            bytes(6, &image_inline),
            bytes(6, &image_existing_uri),
            bytes(6, &image_gone_with_inline),
        ]
        .concat();
        let (missing, val) = projected_combined(&parent, &[9], &[6]).unwrap();
        assert!(!missing);
        let items = val["media"].as_array().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["uri"], "data:image/png;base64,Zm9v");
        assert_eq!(items[1]["uri"], "data:image/jpeg;base64,aGVsbG8=");
        assert_eq!(items[1]["description"], "legacy screenshot");
        assert_eq!(items[2]["uri"], existing);
        assert_eq!(items[3]["uri"], "data:image/gif;base64,aGVsbG8=");

        // Gone local file with empty/absent base64_data reports missing attachment.
        let gone_only = bytes(
            6,
            &[
                bytes(2, b"image/png"),
                bytes(4, b"file:///nonexistent/cv-fixture/missing.png"),
            ]
            .concat(),
        );
        let (missing, val) = projected_combined(&gone_only, &[], &[6]).unwrap();
        assert!(missing);
        assert_eq!(val["media"][0]["uri"], "file:///nonexistent/cv-fixture/missing.png");

        // Malformed base64 characters, bad padding, bad length, or bad MIME fail cleanly.
        for bad in [
            [bytes(1, b"evil\"}"), bytes(2, b"image/png")].concat(),
            [bytes(1, b"abc"), bytes(2, b"image/png")].concat(),
            [bytes(1, b"a==="), bytes(2, b"image/png")].concat(),
            [bytes(1, b"ab=c"), bytes(2, b"image/png")].concat(),
            [bytes(1, b"aGVsbG8="), bytes(2, b"image/png,evil")].concat(),
        ] {
            assert!(projected_combined(&bytes(6, &bad), &[], &[6]).is_err());
        }
    }
}

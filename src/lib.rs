// Copyright 2016 mime-multipart Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

extern crate httparse;
extern crate hyper;
#[macro_use]
extern crate mime;
extern crate tempdir;
extern crate textnonce;
#[macro_use]
extern crate log;
extern crate encoding;
extern crate buf_read_ext;
extern crate bytes;

pub mod error;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use error::Error;

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::borrow::Cow;
use std::ops::Drop;
use encoding::{all, Encoding, DecoderTrap};
use hyper::header::{ContentType, Headers, ContentDisposition, DispositionParam,
                    DispositionType, Charset};
use tempdir::TempDir;
use textnonce::TextNonce;
use buf_read_ext::BufReadExt;
use mime::Mime;

/// A multipart part which is not a file (stored in memory)
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub headers: Headers,
    pub body: Vec<u8>,
}
impl Part {
    /// Mime content-type specified in the header
    pub fn content_type(&self) -> Option<Mime> {
        let ct: Option<&ContentType> = self.headers.get();
        ct.map(|ref ct| ct.0.clone())
    }
}

/// A file that is to be inserted into a `multipart/*` or alternatively an uploaded file that
/// was received as part of `multipart/*` parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct FilePart {
    /// The headers of the part
    pub headers: Headers,
    /// A temporary file containing the file content
    pub path: PathBuf,
    /// Optionally, the size of the file.  This is filled when multiparts are parsed, but is
    /// not necessary when they are generated.
    pub size: Option<usize>,
    // The temporary directory the upload was put into, saved for the Drop trait
    tempdir: Option<PathBuf>,
}
impl FilePart {
    pub fn new(headers: Headers, path: &Path) -> FilePart
    {
        FilePart {
            headers: headers,
            path: path.to_owned(),
            size: None,
            tempdir: None,
        }
    }

    /// If you do not want the file on disk to be deleted when Self drops, call this
    /// function.  It will become your responsability to clean up.
    pub fn do_not_delete_on_drop(&mut self) {
        self.tempdir = None;
    }

    /// Create a new temporary FilePart (when created this way, the file will be
    /// deleted once the FilePart object goes out of scope).
    pub fn create(headers: Headers) -> Result<FilePart, Error> {
        // Setup a file to capture the contents.
        let mut path = try!(TempDir::new("mime_multipart")).into_path();
        let tempdir = Some(path.clone());
        path.push(TextNonce::sized_urlsafe(32).unwrap().into_string());
        Ok(FilePart {
            headers: headers,
            path: path,
            size: None,
            tempdir: tempdir,
        })
    }

    /// Filename that was specified when the file was uploaded.  Returns `Ok<None>` if there
    /// was no content-disposition header supplied.
    pub fn filename(&self) -> Result<Option<String>, Error> {
        let cd: Option<&ContentDisposition> = self.headers.get();
        match cd {
            Some(cd) => get_content_disposition_filename(cd),
            None => Ok(None),
        }
    }

    /// Mime content-type specified in the header
    pub fn content_type(&self) -> Option<Mime> {
        let ct: Option<&ContentType> = self.headers.get();
        ct.map(|ref ct| ct.0.clone())
    }
}
impl Drop for FilePart {
    fn drop(&mut self) {
        if self.tempdir.is_some() {
            let _ = ::std::fs::remove_file(&self.path);
            let _ = ::std::fs::remove_dir(&self.tempdir.as_ref().unwrap());
        }
    }
}

/// A multipart part which could be either a file, in memory, or another multipart
/// container containing nested parts.
#[derive(Clone, Debug)]
pub enum Node {
    /// A part in memory
    Part(Part),
    /// A part streamed to a file
    File(FilePart),
    /// A container of nested multipart parts
    Multipart((Headers, Vec<Node>)),
}

/// Parse a MIME `multipart/*` from a `Read`able stream into a `Vec` of `Node`s, streaming
/// files to disk and keeping the rest in memory.  Recursive `multipart/*` parts will are
/// parsed as well and returned within a `Node::Multipart` variant.
///
/// If `always_use_files` is true, all parts will be streamed to files.  If false, only parts
/// with a `ContentDisposition` header set to `Attachment` or otherwise containing a `Filename`
/// parameter will be streamed to files.
///
/// It is presumed that the headers are still in the stream.  If you have them separately,
/// use `parse_multipart_body()` instead.
pub fn read_multipart<S: Read>(
    stream: &mut S,
    always_use_files: bool)
    -> Result<Vec<Node>, Error>
{
    let mut nodes: Vec<Node> = Vec::new();
    let mut reader = BufReader::with_capacity(4096, stream);

    let mut buf: Vec<u8> = Vec::new();
    let mut header_memory = [httparse::EMPTY_HEADER; 64];

    let (_, found) = try!(reader.stream_until_token(b"\r\n\r\n", &mut buf));
    if ! found { return Err(Error::EofInMainHeaders); }

    // Keep the CRLFCRLF as httparse will expect it
    buf.extend(b"\r\n\r\n".iter().cloned());

    // Parse the headers
    {
    let headers = try!(match httparse::parse_headers(&buf, &mut header_memory) {
        Ok(httparse::Status::Complete((_, raw_headers))) => {
            let mut headers = Headers::new();
            use ::bytes::Bytes as Bs;
            headers.extend(raw_headers.iter().map(|rh| (rh.name, Bs::from(rh.value))));
            Ok(headers)
        },
        Ok(httparse::Status::Partial) => Err(Error::PartialHeaders),
        Err(err) => Err(From::from(err)),
    });

    try!(inner(&mut reader, &headers, &mut nodes, always_use_files));
    }
    Ok(nodes)
}

/// Parse a MIME `multipart/*` from a `Read`able stream into a `Vec` of `Node`s, streaming
/// files to disk and keeping the rest in memory.  Recursive `multipart/*` parts will are
/// parsed as well and returned within a `Node::Multipart` variant.
///
/// If `always_use_files` is true, all parts will be streamed to files.  If false, only parts
/// with a `ContentDisposition` header set to `Attachment` or otherwise containing a `Filename`
/// parameter will be streamed to files.
///
/// It is presumed that you have the `Headers` already and the stream starts at the body.
/// If the headers are still in the stream, use `parse_multipart()` instead.
pub fn read_multipart_body<S: Read>(
    stream: &mut S,
    headers: &Headers,
    always_use_files: bool)
    -> Result<Vec<Node>, Error>
{
    let mut reader = BufReader::with_capacity(4096, stream);
    let mut nodes: Vec<Node> = Vec::new();
    try!(inner(&mut reader, headers, &mut nodes, always_use_files));
    Ok(nodes)
}

fn inner<R: BufRead>(
    reader: &mut R,
    headers: &Headers,
    nodes: &mut Vec<Node>,
    always_use_files: bool)
    -> Result<(), Error>
{
    let mut buf: Vec<u8> = Vec::new();

    let boundary = try!(get_multipart_boundary(headers));

    // Read past the initial boundary
    let (_, found) = try!(reader.stream_until_token(&boundary, &mut buf));
    if ! found { return Err(Error::EofBeforeFirstBoundary); }

    // Define the boundary, including the line terminator preceding it.
    // Use their first line terminator to determine whether to use CRLF or LF.
    let (lt, ltlt, lt_boundary) = {
        let peeker = try!(reader.fill_buf());
        if peeker.len() > 1 && &peeker[..2]==b"\r\n" {
            let mut output = Vec::with_capacity(2 + boundary.len());
            output.push(b'\r');
            output.push(b'\n');
            output.extend(boundary.clone());
            (vec![b'\r', b'\n'], vec![b'\r', b'\n', b'\r', b'\n'], output)
        }
        else if peeker.len() > 0 && peeker[0]==b'\n' {
            let mut output = Vec::with_capacity(1 + boundary.len());
            output.push(b'\n');
            output.extend(boundary.clone());
            (vec![b'\n'], vec![b'\n', b'\n'], output)
        }
        else {
            return Err(Error::NoCrLfAfterBoundary);
        }
    };

    loop {
        // If the next two lookahead characters are '--', parsing is finished.
        {
            let peeker = try!(reader.fill_buf());
            if peeker.len() >= 2 && &peeker[..2] == b"--" {
                return Ok(());
            }
        }

        // Read the line terminator after the boundary
        let (_, found) = try!(reader.stream_until_token(&lt, &mut buf));
        if ! found { return Err(Error::NoCrLfAfterBoundary); }

        // Read the headers (which end in 2 line terminators)
        buf.truncate(0); // start fresh
        let (_, found) = try!(reader.stream_until_token(&ltlt, &mut buf));
        if ! found { return Err(Error::EofInPartHeaders); }

        // Keep the 2 line terminators as httparse will expect it
        buf.extend(ltlt.iter().cloned());

        // Parse the headers
        let part_headers = {
            let mut header_memory = [httparse::EMPTY_HEADER; 4];
            try!(match httparse::parse_headers(&buf, &mut header_memory) {
                Ok(httparse::Status::Complete((_, raw_headers))) => {
                    let mut headers = Headers::new();
                    use ::bytes::Bytes as Bs;
                    headers.extend(raw_headers.iter().map(|rh| (rh.name, Bs::from(rh.value))));
                    Ok(headers)
                },
                Ok(httparse::Status::Partial) => Err(Error::PartialHeaders),
                Err(err) => Err(From::from(err)),
            })
        };

        // Check for a nested multipart
        let nested = {
            let ct: Option<&ContentType> = part_headers.get();
            if let Some(ct) = ct {
                ct.type_() == mime::MULTIPART
            } else {
                false
            }
        };
        if nested {
            // Recurse:
            let mut inner_nodes: Vec<Node> = Vec::new();
            try!(inner(reader, &part_headers, &mut inner_nodes, always_use_files));
            nodes.push(Node::Multipart((part_headers, inner_nodes)));
            continue;
        }

        let is_file = always_use_files || {
            let cd: Option<&ContentDisposition> = part_headers.get();
            if cd.is_some() {
                if cd.unwrap().disposition == DispositionType::Attachment {
                    true
                } else {
                    cd.unwrap().parameters.iter().any(|x| match x {
                        &DispositionParam::Filename(_,_,_) => true,
                        _ => false
                    })
                }
            } else {
                false
            }
        };
        if is_file {
            // Setup a file to capture the contents.
            let mut filepart = try!(FilePart::create(part_headers));
            let mut file = try!(File::create(filepart.path.clone()));

            // Stream out the file.
            let (read, found) = try!(reader.stream_until_token(&lt_boundary, &mut file));
            if ! found { return Err(Error::EofInFile); }
            filepart.size = Some(read);

            // TODO: Handle Content-Transfer-Encoding.  RFC 7578 section 4.7 deprecated
            // this, and the authors state "Currently, no deployed implementations that
            // send such bodies have been discovered", so this is very low priority.

            nodes.push(Node::File(filepart));
        } else {
            buf.truncate(0); // start fresh
            let (_, found) = try!(reader.stream_until_token(&lt_boundary, &mut buf));
            if ! found { return Err(Error::EofInPart); }

            nodes.push(Node::Part(Part {
                headers: part_headers,
                body: buf.clone(),
            }));
        }
    }
}

/// Get the `multipart/*` boundary string from `hyper::Headers`
pub fn get_multipart_boundary(headers: &Headers) -> Result<Vec<u8>, Error> {
    // Verify that the request is 'Content-Type: multipart/*'.
    let ct: &ContentType = match headers.get() {
        Some(ct) => ct,
        None => return Err(Error::NoRequestContentType),
    };
    let &ContentType(ref mime) = ct;

    if mime.type_() != ::mime::MULTIPART {
        return Err(Error::NotMultipart);
    }

    for (attr, ref val) in mime.params() {
        if let ::mime::BOUNDARY = attr {
            let mut boundary = Vec::with_capacity(2 + val.as_ref().len());
            boundary.extend(b"--".iter().cloned());
            boundary.extend(val.as_ref().as_bytes());
            return Ok(boundary);
        }
    }
    Err(Error::BoundaryNotSpecified)
}

#[inline]
fn get_content_disposition_filename(cd: &ContentDisposition) -> Result<Option<String>, Error> {
    if let Some(&DispositionParam::Filename(ref charset, _, ref bytes)) =
        cd.parameters.iter().find(|&x| match *x {
            DispositionParam::Filename(_,_,_) => true,
            _ => false,
        })
    {
        match charset_decode(charset, bytes) {
            Ok(filename) => Ok(Some(filename)),
            Err(e) => Err(Error::Decoding(e)),
        }
    } else {
        Ok(None)
    }
}

// This decodes bytes encoded according to a hyper::header::Charset encoding, using the
// rust-encoding crate.  Only supports encodings defined in both crates.
fn charset_decode(charset: &Charset, bytes: &[u8]) -> Result<String, Cow<'static, str>> {
    Ok(match *charset {
        Charset::Us_Ascii => try!(all::ASCII.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_1 => try!(all::ISO_8859_1.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_2 => try!(all::ISO_8859_2.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_3 => try!(all::ISO_8859_3.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_4 => try!(all::ISO_8859_4.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_5 => try!(all::ISO_8859_5.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_6 => try!(all::ISO_8859_6.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_7 => try!(all::ISO_8859_7.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_8 => try!(all::ISO_8859_8.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_8859_9 => return Err("ISO_8859_9 is not supported".into()),
        Charset::Iso_8859_10 => try!(all::ISO_8859_10.decode(bytes, DecoderTrap::Strict)),
        Charset::Shift_Jis => return Err("Shift_Jis is not supported".into()),
        Charset::Euc_Jp => try!(all::EUC_JP.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_2022_Kr => return Err("Iso_2022_Kr is not supported".into()),
        Charset::Euc_Kr => return Err("Euc_Kr is not supported".into()),
        Charset::Iso_2022_Jp => try!(all::ISO_2022_JP.decode(bytes, DecoderTrap::Strict)),
        Charset::Iso_2022_Jp_2 => return Err("Iso_2022_Jp_2 is not supported".into()),
        Charset::Iso_8859_6_E => return Err("Iso_8859_6_E is not supported".into()),
        Charset::Iso_8859_6_I => return Err("Iso_8859_6_I is not supported".into()),
        Charset::Iso_8859_8_E => return Err("Iso_8859_8_E is not supported".into()),
        Charset::Iso_8859_8_I => return Err("Iso_8859_8_I is not supported".into()),
        Charset::Gb2312 => return Err("Gb2312 is not supported".into()),
        Charset::Big5 => try!(all::BIG5_2003.decode(bytes, DecoderTrap::Strict)),
        Charset::Koi8_R => try!(all::KOI8_R.decode(bytes, DecoderTrap::Strict)),
        Charset::Ext(ref s) => match &**s {
            "UTF-8" => try!(all::UTF_8.decode(bytes, DecoderTrap::Strict)),
            _ => return Err("Encoding is not supported".into()),
        },
    })
}

/// Generate a valid multipart boundary, statistically unlikely to be found within
/// the content of the parts.
pub fn generate_boundary() -> Vec<u8> {
    TextNonce::sized(68).unwrap().into_string().into_bytes()
}

// Convenience method, like write_all(), but returns the count of bytes written.
trait WriteAllCount {
    fn write_all_count(&mut self, buf: &[u8]) -> ::std::io::Result<usize>;
}
impl<T: Write> WriteAllCount for T {
    fn write_all_count(&mut self, buf: &[u8]) -> ::std::io::Result<usize>
    {
        try!(self.write_all(buf));
        Ok(buf.len())
    }
}

/// Stream a multipart body to the output `stream` given, made up of the `parts`
/// given.  Top-level headers are NOT included in this stream; the caller must send
/// those prior to calling write_multipart().
/// Returns the number of bytes written, or an error.
pub fn write_multipart<S: Write>(
    stream: &mut S,
    boundary: &Vec<u8>,
    nodes: &Vec<Node>)
    -> Result<usize, Error>
{
    let mut count: usize = 0;

    for node in nodes {
        // write a boundary
        count += try!(stream.write_all_count(b"--"));
        count += try!(stream.write_all_count(&boundary));
        count += try!(stream.write_all_count(b"\r\n"));

        match node {
            &Node::Part(ref part) => {
                // write the part's headers
                for header in part.headers.iter() {
                    count += try!(stream.write_all_count(header.name().as_bytes()));
                    count += try!(stream.write_all_count(b": "));
                    count += try!(stream.write_all_count(header.value_string().as_bytes()));
                    count += try!(stream.write_all_count(b"\r\n"));
                }

                // write the blank line
                count += try!(stream.write_all_count(b"\r\n"));

                // Write the part's content
                count += try!(stream.write_all_count(&part.body));
            },
            &Node::File(ref filepart) => {
                // write the part's headers
                for header in filepart.headers.iter() {
                    count += try!(stream.write_all_count(header.name().as_bytes()));
                    count += try!(stream.write_all_count(b": "));
                    count += try!(stream.write_all_count(header.value_string().as_bytes()));
                    count += try!(stream.write_all_count(b"\r\n"));
                }

                // write the blank line
                count += try!(stream.write_all_count(b"\r\n"));

                // Write out the files's content
                let mut file = try!(File::open(&filepart.path));
                count += try!(::std::io::copy(&mut file, stream)) as usize;
            },
            &Node::Multipart((ref headers, ref subnodes)) => {
                // Get boundary
                let boundary = try!(get_multipart_boundary(headers));

                // write the multipart headers
                for header in headers.iter() {
                    count += try!(stream.write_all_count(header.name().as_bytes()));
                    count += try!(stream.write_all_count(b": "));
                    count += try!(stream.write_all_count(header.value_string().as_bytes()));
                    count += try!(stream.write_all_count(b"\r\n"));
                }

                // write the blank line
                count += try!(stream.write_all_count(b"\r\n"));

                // Recurse
                count += try!(write_multipart(stream, &boundary, &subnodes));
            },
        }

        // write a line terminator
        count += try!(stream.write_all_count(b"\r\n"));
    }

    // write a final boundary
    count += try!(stream.write_all_count(b"--"));
    count += try!(stream.write_all_count(&boundary));
    count += try!(stream.write_all_count(b"--"));

    Ok(count)
}

pub fn write_chunk<S: Write>(
    stream: &mut S,
    chunk: &[u8]) -> Result<(), ::std::io::Error>
{
    try!(write!(stream, "{:x}\r\n", chunk.len()));
    try!(stream.write_all(chunk));
    try!(stream.write_all(b"\r\n"));
    Ok(())
}

/// Stream a multipart body to the output `stream` given, made up of the `parts`
/// given, using Tranfer-Encoding: Chunked.  Top-level headers are NOT included in this
/// stream; the caller must send those prior to calling write_multipart_chunked().
pub fn write_multipart_chunked<S: Write>(
    stream: &mut S,
    boundary: &Vec<u8>,
    nodes: &Vec<Node>)
    -> Result<(), Error>
{
    for node in nodes {
        // write a boundary
        try!(write_chunk(stream, b"--"));
        try!(write_chunk(stream, &boundary));
        try!(write_chunk(stream, b"\r\n"));

        match node {
            &Node::Part(ref part) => {
                // write the part's headers
                for header in part.headers.iter() {
                    try!(write_chunk(stream, header.name().as_bytes()));
                    try!(write_chunk(stream, b": "));
                    try!(write_chunk(stream, header.value_string().as_bytes()));
                    try!(write_chunk(stream, b"\r\n"));
                }

                // write the blank line
                try!(write_chunk(stream, b"\r\n"));

                // Write the part's content
                try!(write_chunk(stream, &part.body));
            },
            &Node::File(ref filepart) => {
                // write the part's headers
                for header in filepart.headers.iter() {
                    try!(write_chunk(stream, header.name().as_bytes()));
                    try!(write_chunk(stream, b": "));
                    try!(write_chunk(stream, header.value_string().as_bytes()));
                    try!(write_chunk(stream, b"\r\n"));
                }

                // write the blank line
                try!(write_chunk(stream, b"\r\n"));

                // Write out the files's length
                let metadata = try!(::std::fs::metadata(&filepart.path));
                try!(write!(stream, "{:x}\r\n", metadata.len()));

                // Write out the file's content
                let mut file = try!(File::open(&filepart.path));
                try!(::std::io::copy(&mut file, stream)) as usize;
                try!(stream.write(b"\r\n"));
            },
            &Node::Multipart((ref headers, ref subnodes)) => {
                // Get boundary
                let boundary = try!(get_multipart_boundary(headers));

                // write the multipart headers
                for header in headers.iter() {
                    try!(write_chunk(stream, header.name().as_bytes()));
                    try!(write_chunk(stream, b": "));
                    try!(write_chunk(stream, header.value_string().as_bytes()));
                    try!(write_chunk(stream, b"\r\n"));
                }

                // write the blank line
                try!(write_chunk(stream, b"\r\n"));

                // Recurse
                try!(write_multipart_chunked(stream, &boundary, &subnodes));
            },
        }

        // write a line terminator
        try!(write_chunk(stream, b"\r\n"));
    }

    // write a final boundary
    try!(write_chunk(stream, b"--"));
    try!(write_chunk(stream, &boundary));
    try!(write_chunk(stream, b"--"));

    // Write an empty chunk to signal the end of the body
    try!(write_chunk(stream, b""));

    Ok(())
}

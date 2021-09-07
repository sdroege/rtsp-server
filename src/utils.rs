// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[must_use]
pub struct RunOnDrop(Option<Box<dyn FnOnce() + Send>>);

impl RunOnDrop {
    pub fn new<F: FnOnce() + Send + 'static>(func: F) -> RunOnDrop {
        RunOnDrop(Some(Box::new(func)))
    }
}

impl Drop for RunOnDrop {
    fn drop(&mut self) {
        if let Some(func) = self.0.take() {
            func();
        }
    }
}

/// Extension trait for [`url::Url`]
pub trait UrlExt {
    /// Returns `url` relative to `self` if `self` is a base of it.
    fn make_relative(&self, url: &url::Url) -> Option<String>;
}

impl UrlExt for url::Url {
    fn make_relative(&self, url: &url::Url) -> Option<String> {
        if self.cannot_be_a_base() {
            return None;
        }

        // Scheme, host and port need to be the same
        if self.scheme() != url.scheme() || self.host() != url.host() || self.port() != url.port() {
            return None;
        }

        // We ignore username/password at this point

        // The path has to be transformed
        let mut relative = String::new();

        // Extract the filename of both URIs, these need to be handled separately
        fn extract_path_filename(s: &str) -> (&str, &str) {
            let last_slash_idx = s.rfind('/').unwrap_or(0);
            let (path, filename) = s.split_at(last_slash_idx);
            if filename.is_empty() {
                (path, "")
            } else {
                (path, &filename[1..])
            }
        }

        let (base_path, base_filename) = extract_path_filename(self.path());
        let (url_path, url_filename) = extract_path_filename(url.path());

        let mut base_path = base_path.split('/').peekable();
        let mut url_path = url_path.split('/').peekable();

        // Skip over the common prefix
        while base_path.peek().is_some() && base_path.peek() == url_path.peek() {
            base_path.next();
            url_path.next();
        }

        // Add `..` segments for the remainder of the base path
        for base_path_segment in base_path {
            // Skip empty last segments
            if base_path_segment.is_empty() {
                break;
            }

            if !relative.is_empty() {
                relative.push('/');
            }

            relative.push_str("..");
        }

        // Append the remainder of the other URI
        for url_path_segment in url_path {
            if !relative.is_empty() {
                relative.push('/');
            }

            relative.push_str(url_path_segment);
        }

        // Add the filename if they are not the same
        if base_filename != url_filename {
            // If the URIs filename is empty this means that it was a directory
            // so we'll have to append a '/'.
            //
            // Otherwise append it directly as the new filename.
            if url_filename.is_empty() {
                relative.push('/');
            } else {
                if !relative.is_empty() {
                    relative.push('/');
                }
                relative.push_str(url_filename);
            }
        }

        // Query and fragment are only taken from the other URI
        if let Some(query) = url.query() {
            relative.push('?');
            relative.push_str(query);
        }

        if let Some(fragment) = url.fragment() {
            relative.push('#');
            relative.push_str(fragment);
        }

        Some(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_relative() {
        let tests = [
            (
                "rtsp://127.0.0.1:8554/test",
                "rtsp://127.0.0.1:8554/test",
                "",
            ),
            (
                "rtsp://127.0.0.1:8554/test",
                "rtsp://127.0.0.1:8554/test/",
                "test/",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test",
                "../test",
            ),
            (
                "rtsp://127.0.0.1:8554/",
                "rtsp://127.0.0.1:8554/?foo=bar#123",
                "?foo=bar#123",
            ),
            (
                "rtsp://127.0.0.1:8554/",
                "rtsp://127.0.0.1:8554/test/video",
                "test/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test",
                "rtsp://127.0.0.1:8554/test/video",
                "test/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test/video",
                "video",
            ),
            (
                "rtsp://127.0.0.1:8554/test",
                "rtsp://127.0.0.1:8554/test2/video",
                "test2/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test2/video",
                "../test2/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/bla",
                "rtsp://127.0.0.1:8554/test2/video",
                "../test2/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/bla/",
                "rtsp://127.0.0.1:8554/test2/video",
                "../../test2/video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/?foo=bar#123",
                "rtsp://127.0.0.1:8554/test/video",
                "video",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test/video?baz=meh#456",
                "video?baz=meh#456",
            ),
            (
                "rtsp://127.0.0.1:8554/test",
                "rtsp://127.0.0.1:8554/test?baz=meh#456",
                "?baz=meh#456",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test?baz=meh#456",
                "../test?baz=meh#456",
            ),
            (
                "rtsp://127.0.0.1:8554/test/",
                "rtsp://127.0.0.1:8554/test/?baz=meh#456",
                "?baz=meh#456",
            ),
            (
                "rtsp://127.0.0.1:8554/test/?foo=bar#123",
                "rtsp://127.0.0.1:8554/test/video?baz=meh#456",
                "video?baz=meh#456",
            ),
        ];

        for (base, uri, relative) in &tests {
            let base_uri = url::Url::parse(base).unwrap();
            let relative_uri = url::Url::parse(uri).unwrap();
            let make_relative = base_uri.make_relative(&relative_uri).unwrap();
            assert_eq!(
                make_relative, *relative,
                "base: {}, uri: {}, relative: {}",
                base, uri, relative
            );
            assert_eq!(
                base_uri.join(&relative).unwrap().as_str(),
                *uri,
                "base: {}, uri: {}, relative: {}",
                base,
                uri,
                relative
            );
        }
    }
}

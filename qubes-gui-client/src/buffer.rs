/*
 * The Qubes OS Project, https://www.qubes-os.org
 *
 * Copyright (C) 2021  Demi Marie Obenour  <demi@invisiblethingslab.com>
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
 *
 */
//! A wrapper around vchans that provides a write buffer.  Used to prevent
//! deadlocks.

use qubes_castable::Castable as _;
use qubes_gui::Header;
use std::collections::VecDeque;
use std::io::{self, Error, ErrorKind};
use std::mem::size_of;
use std::ops::Range;

#[derive(Debug)]
enum ReadState {
    ReadingHeader,
    ReadingBody(Header, usize),
    Discard(usize),
    Error,
}

// Trait for a vchan, for unit-testing
pub(crate) trait VchanMock {
    fn buffer_space(&self) -> usize;
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn send(&mut self, buf: &[u8]) -> io::Result<usize>;
    fn wait(&self);
    fn data_ready(&self) -> usize;
}

impl VchanMock for vchan::Vchan {
    fn buffer_space(&self) -> usize {
        vchan::Vchan::buffer_space(self)
    }
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        vchan::Vchan::recv(self, buf)
    }
    fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
        vchan::Vchan::send(self, buf)
    }
    fn wait(&self) {
        vchan::Vchan::wait(self)
    }
    fn data_ready(&self) -> usize {
        vchan::Vchan::data_ready(self)
    }
}

#[derive(Debug)]
pub(crate) struct Vchan<T: VchanMock> {
    vchan: T,
    queue: VecDeque<Vec<u8>>,
    offset: usize,
    state: ReadState,
    header: Header,
    buffer: Vec<u8>,
}

#[inline(always)]
fn u32_to_usize(i: u32) -> usize {
    // If u32 doesn’t actually fit in a usize, fail the build
    let [] = [0; if u32::MAX as usize as u32 == u32::MAX {
        0
    } else {
        1
    }];
    i as usize
}

impl<T: VchanMock> Vchan<T> {
    fn write_slice(vchan: &mut T, slice: &[u8]) -> io::Result<usize> {
        let space = vchan.buffer_space();
        if space == 0 {
            Ok(0)
        } else {
            let to_write = space.min(slice.len());
            vchan.send(&slice[..to_write])
        }
    }

    fn drain(&mut self) -> io::Result<usize> {
        let mut written = 0;
        loop {
            let front: &mut _ = match self.queue.front_mut() {
                None => break Ok(written),
                Some(e) => e,
            };
            let to_write = &front[self.offset..];
            if to_write.is_empty() {
                self.queue.pop_front();
                self.offset = 0;
                continue;
            }
            let written_this_time = Self::write_slice(&mut self.vchan, to_write)?;
            written += written_this_time;
            self.offset += written_this_time;
            if written_this_time < to_write.len() {
                break Ok(written);
            }
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.drain()?;
        if !self.queue.is_empty() {
            self.queue.push_back(buf.to_owned());
            return Ok(());
        }
        assert_eq!(self.offset, 0);
        let written = Self::write_slice(&mut self.vchan, buf)?;
        if written != buf.len() {
            assert!(written < buf.len());
            self.queue.push_back(buf[written..].to_owned());
        }
        Ok(())
    }

    #[inline]
    fn recv(&mut self, s: Range<usize>) -> io::Result<usize> {
        self.vchan.recv(&mut self.buffer[s]).map_err(|e| {
            self.state = ReadState::Error;
            e
        })
    }

    pub fn wait(&mut self) {
        self.vchan.wait()
    }

    /// If there is nothing to read, return `Ok(None)` immediately; otherwise,
    /// returns `Ok(Some(msg))` if a complete message has been read, or `Err`
    /// if something went wrong.
    pub fn read_header(&mut self) -> io::Result<Option<(Header, &[u8])>> {
        self.drain()?;
        let mut ready = self.vchan.data_ready();
        loop {
            if ready == 0 {
                break Ok(None);
            }
            match self.state {
                ReadState::Error => {
                    break Err(Error::new(ErrorKind::Other, "Already in error state"))
                }
                ReadState::ReadingHeader if ready >= size_of::<Header>() => {
                    let mut header = <Header as Default>::default();
                    if self.vchan.recv(header.as_mut_bytes()).map_err(|e| {
                        self.state = ReadState::Error;
                        e
                    })? != size_of::<Header>()
                    {
                        break Err(Error::new(
                            ErrorKind::UnexpectedEof,
                            "Failed to read a full message header",
                        ));
                    }
                    ready -= size_of::<Header>();
                    let untrusted_len = u32_to_usize(header.untrusted_len);
                    match qubes_gui::msg_length_limits(header.ty) {
                        // See below comment regarding empty messages!
                        None if untrusted_len == 0 => continue,
                        None => self.state = ReadState::Discard(untrusted_len),
                        Some(max_len) if max_len.contains(&untrusted_len) => {
                            // length was sanitized above
                            self.buffer.resize(untrusted_len, 0);
                            // If the message has an empty body, **do not wait for a body byte to
                            // be sent**, as none will ever arrive.  This will cause the code to
                            // run one message behind, but only for empty messages!
                            if untrusted_len == 0 {
                                self.state = ReadState::ReadingHeader;
                                break Ok(Some((header, &self.buffer[..])));
                            }
                            self.state = ReadState::ReadingBody(header, 0)
                        }
                        Some(_) => {
                            break Err(Error::new(
                                ErrorKind::InvalidData,
                                "Incoming packet has invalid size",
                            ))
                        }
                    }
                }
                ReadState::ReadingHeader => break Ok(None),
                ReadState::Discard(len) => {
                    self.buffer.resize(256.min(len).max(self.buffer.len()), 0);
                    let buf_len = self.buffer.len();
                    let bytes_read = self.recv(0..ready.min(len.min(buf_len) as usize))?;
                    if len == bytes_read {
                        self.state = ReadState::ReadingHeader
                    } else if bytes_read == 0 {
                        break Err(Error::new(ErrorKind::UnexpectedEof, "EOF on the vchan"));
                    } else {
                        assert!(len > bytes_read);
                        self.state = ReadState::Discard(len - bytes_read)
                    }
                }
                ReadState::ReadingBody(header, read_so_far) => {
                    let buffer_len = self.buffer.len();
                    let to_read = ready.min(buffer_len - read_so_far);
                    let bytes_read = self.recv(read_so_far..read_so_far + to_read)?;
                    if bytes_read == to_read {
                        self.state = ReadState::ReadingHeader;
                        break Ok(Some((header, &self.buffer[..])));
                    } else if bytes_read == 0 {
                        break Err(Error::new(ErrorKind::UnexpectedEof, "EOF on the vchan"));
                    } else {
                        assert!(to_read > bytes_read);
                        self.state = ReadState::ReadingBody(header, read_so_far + bytes_read)
                    }
                }
            }
        }
    }
}

impl Vchan<vchan::Vchan> {
    pub fn agent(domain: u16) -> io::Result<(Self, qubes_gui::XConf)> {
        let vchan = vchan::Vchan::server(domain, qubes_gui::LISTENING_PORT.into(), 4096, 4096)?;
        loop {
            match vchan.status() {
                vchan::Status::Waiting => vchan.wait(),
                vchan::Status::Connected => break,
                vchan::Status::Disconnected => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "Didn’t get a connection from the GUI daemon",
                    ))
                }
            }
        }
        let mut res = Self {
            vchan,
            queue: Default::default(),
            offset: 0,
            header: Default::default(),
            state: ReadState::ReadingHeader,
            buffer: vec![],
        };
        res.write(((1u32 << 16) | 3u32).as_bytes())?;
        res.drain()?;
        let mut conf = qubes_gui::XConf::default();
        res.vchan.recv(conf.as_mut_bytes())?;
        Ok((res, conf))
    }

    pub fn daemon(domain: u16) -> io::Result<Self> {
        Ok(Self {
            vchan: vchan::Vchan::client(domain, qubes_gui::LISTENING_PORT.into())?,
            queue: Default::default(),
            offset: 0,
            header: Default::default(),
            state: ReadState::ReadingHeader,
            buffer: vec![],
        })
    }

    pub fn as_raw_fd(&self) -> std::os::raw::c_int {
        self.vchan.fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct MockVchan {
        read_buf: Vec<u8>,
        write_buf: Vec<u8>,
        buffer_space: usize,
        data_ready: usize,
        cursor: usize,
    }

    impl VchanMock for MockVchan {
        fn wait(&self) {}
        fn data_ready(&self) -> usize {
            self.data_ready
        }
        fn buffer_space(&self) -> usize {
            self.buffer_space
        }
        fn send(&mut self, buffer: &[u8]) -> io::Result<usize> {
            assert!(
                buffer.len() <= self.buffer_space,
                "Agents never write more space than is available"
            );
            self.write_buf.extend_from_slice(buffer);
            self.buffer_space -= buffer.len();
            Ok(buffer.len())
        }
        fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(
                self.read_buf.len() >= self.data_ready
                    && self.read_buf.len() - self.data_ready >= self.cursor,
                "mock vchan internal bounds error"
            );
            assert!(
                buffer.len() <= self.data_ready,
                "Agents never read more data than is available"
            );
            buffer.copy_from_slice(&self.read_buf[self.cursor..self.cursor + buffer.len()]);
            self.cursor += buffer.len();
            Ok(buffer.len())
        }
    }
    #[test]
    fn vchan_data() {
        let mock_vchan = MockVchan {
            read_buf: vec![],
            write_buf: vec![],
            buffer_space: 0,
            data_ready: 0,
            cursor: 0,
        };
        let mut under_test = Vchan::<MockVchan> {
            vchan: mock_vchan,
            queue: Default::default(),
            offset: 0,
            header: Default::default(),
            state: ReadState::ReadingHeader,
            buffer: vec![],
        };
        under_test.write(b"test1").unwrap();
        assert_eq!(under_test.queue.len(), 1, "message queued");
        assert_eq!(under_test.queue[0], b"test1");
        assert_eq!(under_test.offset, 0, "no bytes written");
        assert_eq!(under_test.vchan.write_buf, b"", "no bytes written");
        under_test.vchan.buffer_space = 3;
        under_test.drain().expect("drained successfully");
        assert_eq!(under_test.queue.len(), 1);
        assert_eq!(under_test.queue[0], b"test1");
        assert_eq!(under_test.offset, 3);
        assert_eq!(under_test.vchan.write_buf, b"tes");
        assert_eq!(under_test.vchan.buffer_space, 0);
        under_test.vchan.buffer_space = 4;
        under_test.write(b"\0another alpha").unwrap();
        assert_eq!(under_test.queue.len(), 1);
        assert_eq!(under_test.vchan.write_buf, b"test1\0a");
        assert_eq!(
            under_test.queue[0], b"nother alpha",
            "only the minimum number of bytes stored"
        );
        assert_eq!(under_test.offset, 0);
        under_test.vchan.buffer_space = 2;
        under_test.drain().expect("drained successfully");
        assert_eq!(under_test.vchan.write_buf, b"test1\0ano");
        assert_eq!(under_test.vchan.buffer_space, 0);
        under_test.vchan.buffer_space = 7;
        assert_eq!(under_test.read_header().unwrap(), None, "no bytes to read");
        assert_eq!(under_test.vchan.buffer_space, 0);
        assert_eq!(under_test.vchan.write_buf, b"test1\0another al");
        assert_eq!(under_test.queue.len(), 1);
        assert_eq!(under_test.queue[0], b"nother alpha");
        assert_eq!(under_test.offset, 9);
        under_test.vchan.buffer_space = 8;
        under_test.write(b" gamma delta").expect("write works");
        assert_eq!(under_test.vchan.write_buf, b"test1\0another alpha gamm");
        assert_eq!(under_test.offset, 0);
        under_test.write(b" gamma delta").expect("write works");
        under_test.write(b" gamma delta").expect("write works");
        under_test.vchan.buffer_space = 8;
        assert_eq!(under_test.read_header().unwrap(), None, "no bytes to read");
        under_test.vchan.buffer_space = 8;
        assert_eq!(under_test.read_header().unwrap(), None, "no bytes to read");
        under_test.vchan.buffer_space = 8;
        assert_eq!(under_test.read_header().unwrap(), None, "no bytes to read");
        under_test.vchan.buffer_space = 8;
        assert_eq!(under_test.read_header().unwrap(), None, "no bytes to read");
        assert_eq!(
            under_test.vchan.write_buf, b"test1\0another alpha gamma delta gamma delta gamma delta",
            "correct data written"
        );
    }
}

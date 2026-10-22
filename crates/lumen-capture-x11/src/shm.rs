//! MIT-SHM capture: the server writes the root region into a memfd that we read with `pread`.

use std::io;
use std::os::fd::OwnedFd;

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{shm, xproto};
use x11rb::rust_connection::RustConnection;

use crate::Error;

/// A server-attached shared-memory segment sized for one captured region.
#[derive(Debug)]
pub struct Segment {
    seg: shm::Seg,
    memfd: OwnedFd,
    len: usize,
}

impl Segment {
    /// Attach a `len`-byte segment. `None` when MIT-SHM cannot pass file descriptors
    /// (extension missing, version below 1.2, or a remote display refusing the fd).
    pub fn attach(conn: &RustConnection, len: usize) -> Result<Option<Self>, Error> {
        if conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(None);
        }
        let version = shm::query_version(conn)?.reply()?;
        if (version.major_version, version.minor_version) < (1, 2) {
            return Ok(None);
        }
        let memfd = memfd_create(c"lumen-x11-shm", MemfdFlags::CLOEXEC)?;
        ftruncate(&memfd, len as u64)?;
        let seg = conn.generate_id()?;
        let attach = shm::attach_fd(conn, seg, memfd.try_clone()?, false)?.check();
        if attach.is_err() {
            return Ok(None);
        }
        Ok(Some(Self { seg, memfd, len }))
    }

    /// Ask the server to capture `(x, y, w, h)` of `root` into the segment and read it back.
    /// Returns `w * h * 4` bytes of BGRX pixels.
    pub fn grab(
        &self,
        conn: &RustConnection,
        root: xproto::Window,
        origin: (i16, i16),
        size: (u16, u16),
    ) -> Result<Vec<u8>, Error> {
        shm::get_image(
            conn,
            root,
            origin.0,
            origin.1,
            size.0,
            size.1,
            u32::MAX,
            u8::from(xproto::ImageFormat::Z_PIXMAP),
            self.seg,
            0,
        )?
        .reply()?;
        let mut data = vec![0u8; self.len];
        pread_exact(&self.memfd, &mut data)?;
        Ok(data)
    }
}

fn pread_exact(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = rustix::io::pread(fd, &mut buf[done..], done as u64)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        done += n;
    }
    Ok(())
}

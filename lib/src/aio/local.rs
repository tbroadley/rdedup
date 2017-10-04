
use rand;
use rand::Rng;
use INGRESS_BUFFER_SIZE;

use fs2::FileExt;
use sgdata::SGData;
use std::{fs, io, mem};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use walkdir::WalkDir;

use super::{Backend, BackendInstance};
use config;
use super::Metadata;

pub(crate) fn lock_file_path(path: &Path) -> PathBuf {
    path.join(config::LOCK_FILE)
}

pub(crate) struct Local {
    path: PathBuf,
}

struct LocalInstance {
    path: PathBuf,
    rand_ext: String,
}

impl Backend for Local {
    fn new_instance(&self) -> Box<BackendInstance> {
        Box::new(LocalInstance {
            path: self.path.clone(),
            rand_ext: rand::thread_rng()
                .gen_ascii_chars()
                .take(20)
                .collect::<String>(),
        })
    }

    fn lock_exclusive(&self) -> io::Result<fs::File> {
        let lock_path = lock_file_path(&self.path);

        let file = fs::File::create(&lock_path)?;
        file.lock_exclusive()?;

        Ok(file)
    }

    fn lock_shared(&self) -> io::Result<fs::File> {
        let lock_path = lock_file_path(&self.path);

        let file = fs::File::create(&lock_path)?;
        file.lock_shared()?;

        Ok(file)
    }
}

impl Local {
    pub(crate) fn new(path: PathBuf) -> Self {
        Local { path: path }
    }
}

impl BackendInstance for LocalInstance {
    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()> {
        match fs::rename(&src_path, &dst_path) {
            Ok(file) => Ok(file),
            Err(_e) => {
                fs::create_dir_all(dst_path.parent().unwrap())?;
                fs::rename(&src_path, &dst_path)
            }
        }
    }


    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        fs::remove_dir_all(&path)
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        // check if exists on disk
        // remove from `in_progress` if it does
        if idempotent && path.exists() {
            return Ok(());
        }

        let tmp_path = path.with_extension(format!("{}.tmp", self.rand_ext));
        let mut chunk_file = match fs::File::create(&tmp_path) {
            Ok(file) => Ok(file),
            Err(_) => {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::File::create(&tmp_path)
            }
        }?;

        for data_part in sg.as_parts() {
            chunk_file.write_all(data_part)?;
        }

        chunk_file.sync_data()?;
        fs::rename(&tmp_path, &path)?;

        Ok(())
    }


    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        let mut file = fs::File::open(&path)?;

        let mut bufs = Vec::with_capacity(16 * 1024 / INGRESS_BUFFER_SIZE);
        loop {
            let mut buf: Vec<u8> = vec![0u8; INGRESS_BUFFER_SIZE];
            let len = file.read(&mut buf[..])?;

            if len == 0 {
                return Ok(SGData::from_many(bufs));
            }
            buf.truncate(len);
            bufs.push(buf);
        }
    }

    fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        fs::remove_file(&path)
    }


    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        let md = fs::metadata(&path)?;
        Ok(Metadata {
            len: md.len(),
            is_file: md.is_file(),
        })
    }

    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let mut v = Vec::with_capacity(128);

        let dir = fs::read_dir(path);

        match dir {
            Ok(dir) => {
                for entry in dir {
                    let entry = entry?;
                    v.push(entry.path());
                }
                Ok(v)
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }


    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        let path = self.path.join(path);

        if !path.exists() {
            return;
        }

        let mut v = Vec::with_capacity(128);

        for path in WalkDir::new(path) {
            match path {
                Ok(path) => {
                    if !path.file_type().is_file() {
                        continue;
                    }
                    v.push(path.path().into());
                    if v.len() > 100 {
                        tx.send(Ok(mem::replace(&mut v, vec![])))
                            .expect("send failed")
                    }
                }
                Err(e) => tx.send(Err(e.into())).expect("send failed"),
            }
        }
        if !v.is_empty() {
            tx.send(Ok(v)).expect("send failed")
        }
    }
}

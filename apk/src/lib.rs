use crate::compiler::Table;
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};
use xcommon::{Scaler, ZipFileOptions};
use zip::read::ZipFile;
use zip::write::{FileOptions, ZipWriter};

mod compiler;
pub mod manifest;
pub mod res;
mod sign;
mod target;
mod version;

pub use crate::manifest::AndroidManifest;
pub use crate::target::Target;
pub use crate::version::VersionCode;
pub use xcommon::{Certificate, Signer};
pub use zip;

pub struct Apk {
    path: PathBuf,
    zip: ZipWriter<BufWriter<File>>,
}

impl Apk {
    pub fn new(path: PathBuf) -> Result<Self> {
        let zip = ZipWriter::new(BufWriter::new(File::create(&path)?));
        Ok(Self { path, zip })
    }

    pub fn add_res(
        &mut self,
        mut manifest: AndroidManifest,
        icon: Option<&Path>,
        android: &Path,
    ) -> Result<()> {
        let mut buf = vec![];
        let mut table = Table::default();
        table.import_apk(android)?;
        if let Some(path) = icon {
            let mut scaler = Scaler::open(path)?;
            scaler.optimize();
            let mipmap = crate::compiler::compile_mipmap(&manifest.package, "icon")?;

            self.start_file(Path::new("resources.arsc"), ZipFileOptions::Aligned(4))?;
            let mut cursor = Cursor::new(&mut buf);
            mipmap.chunk().write(&mut cursor)?;
            self.zip.write_all(&buf)?;

            for (name, size) in mipmap.variants() {
                buf.clear();
                let mut cursor = Cursor::new(&mut buf);
                scaler.write(&mut cursor, size)?;
                self.start_file(name.as_ref(), ZipFileOptions::Aligned(4))?;
                self.zip.write_all(&buf)?;
            }

            table.import_chunk(mipmap.chunk());
            manifest.application.icon = Some("@mipmap/icon".into());
        }
        let manifest = crate::compiler::compile_manifest(manifest, &table)?;
        self.start_file(Path::new("AndroidManifest.xml"), ZipFileOptions::Compressed)?;
        buf.clear();
        let mut cursor = Cursor::new(&mut buf);
        manifest.write(&mut cursor)?;
        self.zip.write_all(&buf)?;
        Ok(())
    }

    pub fn add_dex(&mut self, dex: &Path) -> Result<()> {
        self.add_file(dex, Path::new("classes.dex"), ZipFileOptions::Compressed)?;
        Ok(())
    }

    pub fn raw_copy_file(&mut self, f: ZipFile) -> Result<()> {
        self.zip.raw_copy_file(f)?;
        Ok(())
    }

    pub fn add_lib(&mut self, target: Target, path: &Path) -> Result<()> {
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid path"))?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid path"))?;
        let mut f = File::open(path)?;
        self.start_file(
            &Path::new("lib").join(target.android_abi()).join(name),
            ZipFileOptions::Compressed,
        )?;
        std::io::copy(&mut f, &mut self.zip)?;
        Ok(())
    }

    pub fn add_file(&mut self, path: &Path, dest: &Path, opts: ZipFileOptions) -> Result<()> {
        let mut f = File::open(path)?;
        self.start_file(dest, opts)?;
        std::io::copy(&mut f, &mut self.zip)?;
        Ok(())
    }

    pub fn add_directory(&mut self, source: &Path, dest: &Path) -> Result<()> {
        add_recursive(self, source, dest)?;
        Ok(())
    }

    fn start_file(&mut self, name: &Path, opts: ZipFileOptions) -> Result<()> {
        let name = name.iter().map(|seg| seg.to_str().unwrap()).collect::<Vec<_>>().join("/");
        let zopts = FileOptions::default().compression_method(opts.compression_method());
        self.zip.start_file_aligned(name, zopts, opts.alignment())?;
        Ok(())
    }

    pub fn finish(mut self, signer: Option<Signer>) -> Result<()> {
        self.zip.finish()?;
        crate::sign::sign(&self.path, signer)?;
        Ok(())
    }

    pub fn sign(path: &Path, signer: Option<Signer>) -> Result<()> {
        crate::sign::sign(&path, signer)
    }

    pub fn verify(path: &Path) -> Result<Vec<Certificate>> {
        crate::sign::verify(path)
    }
}

fn add_recursive(builder: &mut Apk, source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let source = source.join(&file_name);
        let dest = dest.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            add_recursive(builder, &source, &dest)?;
        } else if file_type.is_file() {
            builder.add_file(&source, &dest, ZipFileOptions::Compressed)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::res::Chunk;
    use std::io::{Cursor, Seek, SeekFrom};
    use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};

    pub fn init_logger() -> Result<()> {
        tracing_log::LogTracer::init().ok();
        let env = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_else(|_| "info".to_owned());
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_span_events(FmtSpan::ACTIVE | FmtSpan::CLOSE)
            .with_env_filter(EnvFilter::new(env))
            .with_writer(std::io::stderr)
            .finish();
        tracing::subscriber::set_global_default(subscriber).ok();
        Ok(())
    }

    pub fn find_android_jar() -> Result<PathBuf> {
        let home = std::env::var("ANDROID_HOME")?;
        let platforms = Path::new(&home).join("platforms");
        let mut jar = None;
        for entry in std::fs::read_dir(platforms)? {
            let android = entry?.path().join("android.jar");
            if android.exists() {
                jar = Some(android);
                break;
            }
        }
        Ok(jar.unwrap())
    }

    pub fn android_jar(platform: u16) -> Result<PathBuf> {
        let home = std::env::var("ANDROID_HOME")?;
        let android = Path::new(&home)
            .join("platforms")
            .join(format!("android-{}", platform))
            .join("android.jar");
        Ok(android)
    }

    #[test]
    fn test_bxml_parse_manifest() -> Result<()> {
        const BXML: &[u8] = include_bytes!("../../assets/AndroidManifest.bxml");
        let mut r = Cursor::new(BXML);
        let chunk = Chunk::parse(&mut r)?;
        let pos = r.seek(SeekFrom::Current(0))?;
        assert_eq!(pos, BXML.len() as u64);
        println!("{:#?}", chunk);
        assert!(false);
        Ok(())
    }

    /*#[test]
    fn test_bxml_gen_manifest() -> Result<()> {
        const XML: &str = include_str!("../../assets/AndroidManifest.xml");
        let bxml = Xml::new(XML.to_string()).compile()?;
        let mut cursor = Cursor::new(bxml.as_slice());
        let chunk = Chunk::parse(&mut cursor).unwrap();
        let pos = cursor.seek(SeekFrom::Current(0))?;
        assert_eq!(pos, bxml.len() as u64);
        println!("{:#?}", chunk);
        assert!(false);
        Ok(())
    }*/

    #[test]
    fn test_bxml_parse_arsc() -> Result<()> {
        const BXML: &[u8] = include_bytes!("../../assets/resources.arsc");
        let mut r = Cursor::new(BXML);
        let chunk = Chunk::parse(&mut r)?;
        let pos = r.seek(SeekFrom::Current(0))?;
        assert_eq!(pos, BXML.len() as u64);
        println!("{:#?}", chunk);
        assert!(false);
        Ok(())
    }
}

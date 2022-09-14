//! Remaking of the `file_io` example using the `Adaptor` type.

use async_filelike::Adaptor;
use futures_lite::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    async_io::block_on(async {
        // TODO: handle file opening in-crate
        let file = blocking::unblock(|| std::fs::File::create("foo.txt")).await?;
        let mut file = Adaptor::new(file);

        // Write some data to the file
        file.write_all(b"Hello, world!").await?;

        // Close and reopen the file.
        drop(file);
        let file = blocking::unblock(|| std::fs::File::open("foo.txt")).await?;
        let mut file = Adaptor::new(file);

        // Read the data back.
        let mut buf = [0u8; 13];
        file.read_exact(&mut buf).await?;

        // Print the data.
        println!("{}", String::from_utf8_lossy(&buf[..]));

        // Close the file and delete it.
        drop(file);
        blocking::unblock(|| std::fs::remove_file("foo.txt")).await?;

        Ok(())
    })
}
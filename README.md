# async-filelike

Asynchronous operations on file-like objects.

For socket-like system resources, the [`async-io`] crate provides a wrapper that allows the resource to be used in asynchronous programs. However, this strategy does not work with file-like objects, such as files, pipes and the standard output. Usually, the strategy is to offload these objects onto a [thread pool]. However, operating systems *do* provide APIs in certain cases for asynchronous file I/O; they're just difficult to fit into the asynchronous object model. 

This crate provides wrappers around file-like objects that allows them to be used in asynchronous programs; in many cases, without thread pools.

## License

This project is licensed under one of the following licenses:

  * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
    http://www.apache.org/licenses/LICENSE-2.0)
  * MIT license ([LICENSE-MIT](LICENSE-MIT) or
    http://opensource.org/licenses/MIT)

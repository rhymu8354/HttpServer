# HttpServer (rhymuweb-server)

This is a library which can be used to serve web resources using Hypertext
Transfer Protocol (HTTP).

[![Crates.io](https://img.shields.io/crates/v/rhymuweb-server.svg)](https://crates.io/crates/rhymuweb-server)
[![Documentation](https://docs.rs/rhymuweb-server/badge.svg)][dox]

More information can be found in the [crate documentation][dox].

[dox]: https://docs.rs/rhymuweb-server

## Usage

The [`HttpServer`] type operates as an HTTP server as described in [IETF RFC
7230](https://tools.ietf.org/html/rfc7230).  It maintains set of resource
handlers and a transport layer (typically TCP) listener.  In response to
incoming connections, the server parses data from the connections as HTTP
requests (represented by a
[`rhymuweb::Request`](https://docs.rs/rhymuweb/1.0.0/rhymuweb/struct.Request.html)),
uses the resource handlers to produce responses based on these requests
(represented by a [`rhymuweb::Response`](https://docs.rs/rhymuweb/1.0.0/rhymuweb/struct.Response.html))

Along with the library is an example program, `jeeves`, which demonstrates how
to use [`HttpServer`], greeting any web clients with a friendly response.

[`HttpServer`]: https://docs.rs/rhymuweb-server/1.0.0/rhymuweb-server/struct.HttpServer.html

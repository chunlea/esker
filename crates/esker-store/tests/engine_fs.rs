//! The filesystem the engine runs on is the caller's choice, and the store honours it.
//!
//! [`StoreOptions::fs`] exists so that a store can be opened on something other than the local
//! disk — a tiered filesystem with the database directory as a cache in front of object storage
//! (`docs/DESIGN.md` §13), or an in-memory one for a test that wants to injure it. The option
//! would be easy to add and quietly ignore: `Db::open_with` took a `LocalFileSystem` built on
//! the spot, and a store that kept doing that would still pass every other test in this crate,
//! because a local filesystem is what they all want anyway.
//!
//! So this asserts the negative that catches it: after a write and a flush, the **directory on
//! disk is empty**, because every byte went through the filesystem it was handed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::memfs::MemFileSystem;
use esker_proto::{Epoch, RawKvReq, RawKvResp, RequestHeader};
use esker_store::{Store, StoreOptions};
use tempfile::TempDir;

#[test]
fn a_store_opens_on_the_filesystem_it_is_given() {
    let dir = TempDir::new().unwrap();
    let fs = Arc::new(MemFileSystem::new());
    let store = Store::open(
        dir.path(),
        StoreOptions {
            fs: Arc::clone(&fs) as Arc<dyn esker_engine::FileSystem>,
            ..StoreOptions::new()
        },
    )
    .expect("the store opens on an in-memory filesystem");

    let header = RequestHeader::new(1, Epoch::INITIAL, 0);
    let put = store
        .handle(header, RawKvReq::put(b"k".as_slice(), b"v".as_slice()))
        .expect("the write is served");
    assert_eq!(put, RawKvResp::Put);
    store.flush().expect("the memtable reaches an SST");

    let got = store
        .handle(header, RawKvReq::get(b"k".as_slice()))
        .expect("the read is served");
    assert_eq!(
        got,
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"v"))
        }
    );

    // The negative that makes the positive mean something: nothing was written to the real
    // directory, so the bytes went where the caller said and not where the store felt like.
    let on_disk: Vec<_> = std::fs::read_dir(dir.path())
        .expect("the directory exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        on_disk.is_empty(),
        "the store wrote to the local disk despite being given another filesystem: {on_disk:?}"
    );
}

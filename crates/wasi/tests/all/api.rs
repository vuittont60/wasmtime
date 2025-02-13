use anyhow::Result;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::preview2::bindings::wasi::clocks::wall_clock;
use wasmtime_wasi::preview2::bindings::wasi::filesystem::types as filesystem;
use wasmtime_wasi::preview2::command::{add_to_linker, Command};
use wasmtime_wasi::preview2::{
    self, DirPerms, FilePerms, HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiView,
};

struct CommandCtx {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for CommandCtx {
    fn table(&self) -> &ResourceTable {
        &self.table
    }
    fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
    fn ctx(&self) -> &WasiCtx {
        &self.wasi
    }
    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

use test_programs_artifacts::*;

foreach_api!(assert_test_exists);

async fn instantiate(path: &str, ctx: CommandCtx) -> Result<(Store<CommandCtx>, Command)> {
    let mut config = Config::new();
    config.async_support(true).wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, ctx);
    let component = Component::from_file(&engine, path)?;
    let (command, _instance) = Command::instantiate_async(&mut store, &component, &linker).await?;
    Ok((store, command))
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn api_time() -> Result<()> {
    struct FakeWallClock;

    impl HostWallClock for FakeWallClock {
        fn resolution(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn now(&self) -> Duration {
            Duration::new(1431648000, 100)
        }
    }

    struct FakeMonotonicClock {
        now: Mutex<u64>,
    }

    impl HostMonotonicClock for FakeMonotonicClock {
        fn resolution(&self) -> u64 {
            1_000_000_000
        }

        fn now(&self) -> u64 {
            let mut now = self.now.lock().unwrap();
            let then = *now;
            *now += 42 * 1_000_000_000;
            then
        }
    }

    let table = ResourceTable::new();
    let wasi = WasiCtxBuilder::new()
        .monotonic_clock(FakeMonotonicClock { now: Mutex::new(0) })
        .wall_clock(FakeWallClock)
        .build();

    let (mut store, command) = instantiate(API_TIME_COMPONENT, CommandCtx { table, wasi }).await?;

    command
        .wasi_cli_run()
        .call_run(&mut store)
        .await?
        .map_err(|()| anyhow::anyhow!("command returned with failing exit status"))
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn api_read_only() -> Result<()> {
    let dir = tempfile::tempdir()?;

    std::fs::File::create(dir.path().join("bar.txt"))?.write_all(b"And stood awhile in thought")?;
    std::fs::create_dir(dir.path().join("sub"))?;

    let table = ResourceTable::new();
    let open_dir = Dir::open_ambient_dir(dir.path(), ambient_authority())?;
    let wasi = WasiCtxBuilder::new()
        .preopened_dir(open_dir, DirPerms::READ, FilePerms::READ, "/")
        .build();

    let (mut store, command) =
        instantiate(API_READ_ONLY_COMPONENT, CommandCtx { table, wasi }).await?;

    command
        .wasi_cli_run()
        .call_run(&mut store)
        .await?
        .map_err(|()| anyhow::anyhow!("command returned with failing exit status"))
}

// This is tested in the wasi-http crate, but need to satisfy the `foreach_api!`
// macro above.
#[allow(dead_code)]
fn api_proxy() {}

// This is tested in the wasi-http crate, but need to satisfy the `foreach_api!`
// macro above.
#[allow(dead_code)]
fn api_proxy_streaming() {}

wasmtime::component::bindgen!({
    world: "test-reactor",
    async: true,
    with: {
       "wasi:io/streams": preview2::bindings::io::streams,
       "wasi:filesystem/types": preview2::bindings::filesystem::types,
       "wasi:filesystem/preopens": preview2::bindings::filesystem::preopens,
       "wasi:cli/environment": preview2::bindings::cli::environment,
       "wasi:cli/exit": preview2::bindings::cli::exit,
       "wasi:cli/stdin": preview2::bindings::cli::stdin,
       "wasi:cli/stdout": preview2::bindings::cli::stdout,
       "wasi:cli/stderr": preview2::bindings::cli::stderr,
       "wasi:cli/terminal_input": preview2::bindings::cli::terminal_input,
       "wasi:cli/terminal_output": preview2::bindings::cli::terminal_output,
       "wasi:cli/terminal_stdin": preview2::bindings::cli::terminal_stdin,
       "wasi:cli/terminal_stdout": preview2::bindings::cli::terminal_stdout,
       "wasi:cli/terminal_stderr": preview2::bindings::cli::terminal_stderr,
    },
    ownership: Borrowing {
        duplicate_if_necessary: false
    }
});

#[test_log::test(tokio::test)]
async fn api_reactor() -> Result<()> {
    let table = ResourceTable::new();
    let wasi = WasiCtxBuilder::new().env("GOOD_DOG", "gussie").build();

    let mut config = Config::new();
    config.async_support(true).wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, CommandCtx { table, wasi });
    let component = Component::from_file(&engine, API_REACTOR_COMPONENT)?;
    let (reactor, _instance) =
        TestReactor::instantiate_async(&mut store, &component, &linker).await?;

    // Show that integration with the WASI context is working - the guest will
    // interpolate $GOOD_DOG to gussie here using the environment:
    let r = reactor
        .call_add_strings(&mut store, &["hello", "$GOOD_DOG"])
        .await?;
    assert_eq!(r, 2);

    let contents = reactor.call_get_strings(&mut store).await?;
    assert_eq!(contents, &["hello", "gussie"]);

    // Show that we can pass in a resource type whose impls are defined in the
    // `host` and `wasi-common` crate.
    // Note, this works because of the add_to_linker invocations using the
    // `host` crate for `streams`, not because of `with` in the bindgen macro.
    let writepipe = preview2::pipe::MemoryOutputPipe::new(4096);
    let stream: preview2::OutputStream = Box::new(writepipe.clone());
    let table_ix = store.data_mut().table_mut().push(stream)?;
    let r = reactor.call_write_strings_to(&mut store, table_ix).await?;
    assert_eq!(r, Ok(()));

    assert_eq!(writepipe.contents().as_ref(), b"hellogussie");

    // Show that the `with` invocation in the macro means we get to re-use the
    // type definitions from inside the `host` crate for these structures:
    let ds = filesystem::DescriptorStat {
        data_access_timestamp: Some(wall_clock::Datetime {
            nanoseconds: 123,
            seconds: 45,
        }),
        data_modification_timestamp: Some(wall_clock::Datetime {
            nanoseconds: 789,
            seconds: 10,
        }),
        link_count: 0,
        size: 0,
        status_change_timestamp: Some(wall_clock::Datetime {
            nanoseconds: 0,
            seconds: 1,
        }),
        type_: filesystem::DescriptorType::Unknown,
    };
    let expected = format!("{ds:?}");
    let got = reactor.call_pass_an_imported_record(&mut store, ds).await?;
    assert_eq!(expected, got);

    Ok(())
}

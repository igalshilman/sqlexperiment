use super::*;

#[test]
fn snapshot_abi_rejects_state_outside_memory_and_stack_pointer() -> Result<()> {
    validate_snapshot_abi(quickjs::WASM)?;
    for extra in [
        "(global (mut i32) (i32.const 7))",
        "(table 1 funcref) (func i32.const 0 ref.null func table.set 0)",
        "(memory 1) (data $d \"abc\") (func data.drop $d)",
    ] {
        let wasm = wat::parse_str(format!(
            "(module (global (export \"__stack_pointer\") (mut i32) (i32.const 1024)) {extra})"
        ))?;
        assert!(validate_snapshot_abi(&wasm).is_err(), "{extra}");
    }
    Ok(())
}

/// A module split in two lays out the memory it would have instantiated as
/// built, and runs nothing before that memory is mapped.
#[test]
fn split_lays_out_the_memory_the_module_would_have() -> Result<()> {
    let wasm = wat::parse_str(
        r#"(module (memory (export "memory") 3)
            (data (i32.const 16) "abc") (data (i32.const 70000) "far")
            (data $passive "never"))"#,
    )?;
    let (code, data) = split(&wasm)?;
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let built = Instance::new(&mut store, &Module::new(&engine, &wasm)?, &[])?;
    let built = built.get_memory(&mut store, "memory").unwrap();
    assert_eq!(data.len(), 2 * 65536, "up to the last segment's page");
    assert_eq!(data[..], built.data(&store)[..data.len()]);
    assert!(
        built.data(&store)[data.len()..]
            .iter()
            .all(|&byte| byte == 0)
    );
    let mut store = Store::new(&engine, ());
    let bare = Instance::new(&mut store, &Module::new(&engine, &code)?, &[])?;
    let bare = bare.get_memory(&mut store, "memory").unwrap();
    assert!(bare.data(&store).iter().all(|&byte| byte == 0));
    let started = wat::parse_str("(module (memory 1) (func $f) (start $f))")?;
    assert!(split(&started).is_err());
    Ok(())
}

/// A runtime of each kind this host supports: barriers always, native
/// tracking where the kernel has it.
fn runtimes() -> Result<Vec<Runtime>> {
    let mut runtimes = vec![Runtime::tracked(false)?];
    let native = Runtime::tracked(true)?;
    if native.native_tracking() {
        runtimes.push(native);
    }
    Ok(runtimes)
}

#[tokio::test]
async fn host_writes_are_tracked_until_the_next_commit() -> Result<()> {
    for runtime in runtimes()? {
        let (image, _) = runtime.golden(
            Language::JavaScript,
            "host",
            "export async function main() { return 1; }",
        )?;
        let mut vm = Vm::new(&runtime, Language::JavaScript, "host", 1).await?;
        vm.restore(&image).await?;
        vm.committed()?;
        // No guest allocator call: these must come from the host write path.
        vm.write(4095, &[3, 7])?;
        assert_eq!(vm.written()?, [0, 1]);
        vm.write(8192, &[])?;
        assert_eq!(vm.written()?, [0, 1]);
        vm.write(8192, &[9])?;
        assert_eq!(vm.written()?, [0, 1, 2]);
        // Restore starts afresh. Runtime rebinding may write its own pages,
        // but the discarded turn's must not survive.
        vm.restore(&image).await?;
        assert!(!vm.written()?.contains(&2));
        assert_eq!(vm.memory.data(&vm.store)[8192], image.page(2)[0]);
    }
    Ok(())
}

#[tokio::test]
#[cfg(feature = "verify-dirty")]
async fn verification_oracle_detects_an_untracked_write() -> Result<()> {
    for runtime in runtimes()? {
        let (image, _) = runtime.golden(
            Language::JavaScript,
            "oracle",
            "export async function main() { return 1; }",
        )?;
        let mut vm = Vm::new(&runtime, Language::JavaScript, "oracle", 1).await?;
        vm.restore(&image).await?;
        // A write the tracker then forgets, as if it had missed it.
        let offset = image.len() - 1;
        let byte = image.page((offset / PAGE) as u32)[PAGE - 1];
        vm.memory.write(&mut vm.store, offset, &[byte ^ 1])?;
        vm.committed()?;
        let error = vm
            .capture(&image)
            .await
            .err()
            .expect("missed write must fail");
        assert!(error.to_string().contains("missed page"));
    }
    Ok(())
}

/// A VM started from a golden image leaves its slot as Wasmtime gave it: the
/// next VM there sees none of its writes, neither over golden nor in memory it
/// grew past golden.
#[tokio::test]
async fn a_vm_from_golden_leaves_nothing_behind() -> Result<()> {
    for runtime in runtimes()? {
        let (image, _) = runtime.golden(
            Language::JavaScript,
            "hygiene",
            "export async function main() { return 1; }",
        )?;
        let golden = image.golden_memory().bytes().to_vec();
        let fresh = |vm: Vm| vm.memory.data(&vm.store).to_vec();
        let before = fresh(Vm::new(&runtime, Language::JavaScript, "fresh", 1).await?);
        for _ in 0..3 {
            let mut vm =
                Vm::for_image(&runtime, Language::JavaScript, &image, "hygiene", 1).await?;
            vm.restore(&image).await?;
            assert_eq!(&vm.memory.data(&vm.store)[..golden.len()], &golden[..]);
            assert!(
                vm.memory.data(&vm.store)[golden.len()..]
                    .iter()
                    .all(|byte| *byte == 0)
            );
            // Write over golden, and past it.
            vm.write(PAGE, &[0xAB; 64])?;
            vm.memory.grow(&mut vm.store, 2)?;
            let end = vm.memory.data_size(&vm.store);
            vm.write(end - PAGE, &[0xCD; 64])?;
        }
        // A VM of the module as built, in any slot, starts as before.
        let after = fresh(Vm::new(&runtime, Language::JavaScript, "fresh", 1).await?);
        assert!(after == before, "A fresh VM saw an earlier VM's memory");
    }
    Ok(())
}

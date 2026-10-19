use super::*;

#[test]
fn snapshot_abi_rejects_state_outside_memory_and_stack_pointer() -> Result<()> {
    validate_snapshot_abi(WASM)?;
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
        let (image, _) = runtime.golden("host", "export async function main() { return 1; }")?;
        let mut vm = Vm::new(&runtime, "host", 1).await?;
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
        let (image, _) = runtime.golden("oracle", "export async function main() { return 1; }")?;
        let mut vm = Vm::new(&runtime, "oracle", 1).await?;
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

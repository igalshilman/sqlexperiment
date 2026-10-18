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

#[test]
fn host_ranges_and_epoch_reset() -> Result<()> {
    let runtime = Runtime::new()?;
    let image = runtime.golden("host", "async function main() { return 1; }")?;
    let mut vm = Vm::new(&runtime, "host", 1)?;
    vm.restore(&image)?;
    vm.dirty.data_mut(&mut vm.store).fill(0);
    // No guest allocator call: these marks must come from the host write path.
    vm.write(4095, &[3, 7])?;
    assert_eq!(&vm.dirty.data(&vm.store)[..3], &[1, 1, 0]);
    vm.write(8192, &[])?;
    assert_eq!(vm.dirty.data(&vm.store)[2], 0);
    vm.write(8192, &[9])?;
    assert_eq!(vm.dirty.data(&vm.store)[2], 1);
    // Restore starts a fresh epoch. Runtime rebinding may mark its own pages,
    // but unrelated pages from the discarded turn must not survive.
    vm.restore(&image)?;
    assert_eq!(vm.dirty.data(&vm.store)[2], 0);
    assert_eq!(vm.memory.data(&vm.store)[8192], image.memory[8192]);
    Ok(())
}

#[test]
#[cfg(feature = "verify-dirty")]
fn verification_oracle_detects_an_unmarked_host_write() -> Result<()> {
    let runtime = Runtime::new()?;
    let image = runtime.golden("oracle", "async function main() { return 1; }")?;
    let mut vm = Vm::new(&runtime, "oracle", 1)?;
    vm.restore(&image)?;
    // Deliberately bypass the sole host write helper to prove the oracle works.
    let offset = image.memory.len() - 1;
    vm.memory
        .write(&mut vm.store, offset, &[image.memory[offset] ^ 1])?;
    vm.dirty.data_mut(&mut vm.store)[offset / PAGE] = 0;
    let error = vm.capture(&image).err().expect("missed write must fail");
    assert!(error.to_string().contains("missed page"));
    Ok(())
}

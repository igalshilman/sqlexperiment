use crate::instrument::{EXPORT, instrument};
use anyhow::Result;
use wasmtime::{Engine, Instance, Memory, Module, Store, Val};

struct Pair {
    original: Instance,
    tracked: Instance,
    store: Store<()>,
    old_memory: Memory,
    memory: Memory,
    bitmap: Memory,
}
impl Pair {
    fn new(wat: &str) -> Result<Self> {
        let wasm = wat::parse_str(wat)?;
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());
        let original = Instance::new(&mut store, &Module::new(&engine, &wasm)?, &[])?;
        let tracked = Instance::new(&mut store, &Module::new(&engine, instrument(&wasm)?)?, &[])?;
        let old_memory = original.get_memory(&mut store, "memory").unwrap();
        let memory = tracked.get_memory(&mut store, "memory").unwrap();
        let bitmap = tracked.get_memory(&mut store, EXPORT).unwrap();
        Ok(Self {
            original,
            tracked,
            store,
            old_memory,
            memory,
            bitmap,
        })
    }
    fn call(&mut self, name: &str, args: &[Val], traps: bool) -> Result<()> {
        self.bitmap.data_mut(&mut self.store).fill(0);
        let a = self.original.get_func(&mut self.store, name).unwrap().call(
            &mut self.store,
            args,
            &mut [],
        );
        let b = self.tracked.get_func(&mut self.store, name).unwrap().call(
            &mut self.store,
            args,
            &mut [],
        );
        assert_eq!(a.is_err(), traps, "original result {a:?}");
        assert_eq!(b.is_err(), traps, "tracked result {b:?}");
        assert_eq!(
            self.memory.data(&self.store),
            self.old_memory.data(&self.store)
        );
        Ok(())
    }
    fn marked(&self) -> Vec<usize> {
        self.bitmap
            .data(&self.store)
            .iter()
            .enumerate()
            .filter_map(|(i, b)| (*b != 0).then_some(i))
            .collect()
    }
}

#[test]
fn scalar_writes_offsets_cross_pages_and_traps() -> Result<()> {
    let stores = [
        ("i32.store8", "i32", Val::I32(17), 1),
        ("i32.store16", "i32", Val::I32(12345), 2),
        ("i32.store", "i32", Val::I32(1234567), 4),
        ("i64.store8", "i64", Val::I64(23), 1),
        ("i64.store16", "i64", Val::I64(12345), 2),
        ("i64.store32", "i64", Val::I64(1234567), 4),
        ("i64.store", "i64", Val::I64(1234567890), 8),
        ("f32.store", "f32", Val::F32(1.25f32.to_bits()), 4),
        ("f64.store", "f64", Val::F64(2.75f64.to_bits()), 8),
    ];
    for (op, ty, value, width) in stores {
        let mut p = Pair::new(&format!(
            "(module (memory (export \"memory\") 2 3) (func (export \"write\") (param i32 {ty}) (local i32 i64 f32 f64) local.get 0 local.get 1 {op} offset=7))"
        ))?;
        for start in [0, 4095, 4096, 65535, 131072 - width] {
            p.call("write", &[Val::I32(start - 7), value], start < 7)?;
            if start < 7 {
                assert!(p.marked().is_empty());
                continue;
            }
            let first = start as usize / 4096;
            let last = (start + width - 1) as usize / 4096;
            assert_eq!(
                p.marked(),
                (first..=last).collect::<Vec<_>>(),
                "{op} at {start}"
            );
        }
        p.call("write", &[Val::I32(131072), value], true)?;
        assert!(p.marked().is_empty());
        // Effective address calculation must not wrap to page zero.
        p.call("write", &[Val::I32(-1), value], true)?;
        assert!(p.marked().is_empty());
    }
    Ok(())
}

#[test]
fn bulk_copy_fill_init_zero_lengths_and_overlaps() -> Result<()> {
    let mut p = Pair::new(
        r#"(module
        (memory (export "memory") 2 4)
        (data (i32.const 4090) "abcdefghijklmnop")
        (data $passive "abcdefghijklmnop")
        (func (export "copy") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.copy)
        (func (export "fill") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.fill)
        (func (export "init") (param i32 i32 i32) local.get 0 local.get 1 local.get 2 memory.init $passive))"#,
    )?;
    for name in ["copy", "fill", "init"] {
        let second = if name == "copy" { 4090 } else { 0 };
        p.call(
            name,
            &[Val::I32(4094), Val::I32(second), Val::I32(16)],
            false,
        )?;
        assert_eq!(p.marked(), vec![0, 1]);
        p.call(
            name,
            &[Val::I32(131072), Val::I32(second), Val::I32(0)],
            false,
        )?;
        assert!(p.marked().is_empty());
        p.call(
            name,
            &[Val::I32(131072), Val::I32(second), Val::I32(1)],
            true,
        )?;
        assert!(p.marked().is_empty());
    }
    p.call("fill", &[Val::I32(1), Val::I32(0), Val::I32(16384)], false)?;
    assert_eq!(p.marked(), vec![0, 1, 2, 3, 4]); // writing zeros still marks
    p.call("copy", &[Val::I32(2), Val::I32(1), Val::I32(16384)], false)?;
    assert_eq!(p.marked(), vec![0, 1, 2, 3, 4]);
    Ok(())
}

#[test]
fn growth_and_failed_growth_keep_memory_semantics() -> Result<()> {
    let mut p = Pair::new(
        r#"(module (memory (export "memory") 1 2)
        (func (export "grow") i32.const 1 memory.grow drop)
        (func (export "write") i32.const 131071 i32.const 7 i32.store8))"#,
    )?;
    p.call("grow", &[], false)?;
    assert_eq!(p.memory.data_size(&p.store), 131072);
    assert!(p.marked().is_empty()); // zero-filled growth needs size, not payload
    p.call("write", &[], false)?;
    assert_eq!(p.marked(), vec![31]);
    p.call("grow", &[], false)?;
    assert_eq!(p.memory.data_size(&p.store), 131072);
    Ok(())
}

#[test]
fn mutation_without_allocation_is_tracked() -> Result<()> {
    let mut p = Pair::new(
        r#"(module (memory (export "memory") 1)
        (global $allocations (mut i32) (i32.const 0))
        (func (export "allocate") global.get $allocations i32.const 1 i32.add global.set $allocations)
        (func (export "mutate") i32.const 8192 i32.const 42 i32.store)
        (export "allocations" (global $allocations)))"#,
    )?;
    p.call("allocate", &[], false)?;
    assert!(p.marked().is_empty());
    p.call("mutate", &[], false)?;
    assert_eq!(p.marked(), vec![2]);
    assert_eq!(
        p.tracked
            .get_global(&mut p.store, "allocations")
            .unwrap()
            .get(&mut p.store)
            .i32(),
        Some(1)
    );
    Ok(())
}

#[test]
fn unsupported_memory_modes_and_instructions_fail_closed() -> Result<()> {
    for wat in [
        "(module (memory (export \"memory\") 1 2 shared))",
        "(module (memory (export \"memory\") i64 1))",
        "(module (import \"x\" \"m\" (memory 1)) (export \"memory\" (memory 0)))",
        "(module (memory (export \"memory\") 1) (memory 1))",
        "(module (memory (export \"memory\") 1) (func i32.const 0 v128.const i32x4 0 0 0 0 v128.store))",
        "(module (memory (export \"__durable_dirty_pages\") 1))",
    ] {
        assert!(
            instrument(&wat::parse_str(wat)?).is_err(),
            "Accepted unsupported module {wat}"
        );
    }
    Ok(())
}

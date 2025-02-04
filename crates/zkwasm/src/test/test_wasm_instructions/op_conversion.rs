use crate::test::test_circuit_noexternal;

#[test]
fn test_i32_wrap_i64_ok() {
    let textual_repr = r#"
        (module
            (func (export "test")
                (i64.const 0)
                (i32.wrap_i64)
                (drop)
                (i64.const 0xffffffff00000000)
                (i32.wrap_i64)
                (drop)
                (i64.const 0xfffffffff0f0f0f0)
                (i32.wrap_i64)
                (drop)
                )
            )
        "#;

    test_circuit_noexternal(textual_repr).unwrap()
}

#[test]
fn test_i64_extend_i32_u_ok() {
    let textual_repr = r#"
        (module
            (func (export "test")
                (i32.const 0)
                (i64.extend_i32_u)
                (drop)
                (i32.const -1)
                (i64.extend_i32_u)
                (drop)
            )
        )
        "#;

    test_circuit_noexternal(textual_repr).unwrap()
}

#[test]
fn test_i64_extend_i32_s_ok() {
    let textual_repr = r#"
        (module
            (func (export "test")
                (i32.const 0)
                (i64.extend_i32_s)
                (drop)

                (i32.const 0x7fffffff)
                (i64.extend_i32_s)
                (drop)

                (i32.const -1)
                (i64.extend_i32_s)
                (drop)

                (i32.const 0xffffffff)
                (i64.extend_i32_s)
                (drop)
            )
        )
        "#;

    test_circuit_noexternal(textual_repr).unwrap()
}

// issue 10882
module 0x42::struct_type_parameters_mismatch {
    struct A<Feature> has key {
        root: address
    }
    fun foo<Feature>(addr: address): address
    acquires A {
        borrow_global<A<Feature>>(addr).root
    }
}

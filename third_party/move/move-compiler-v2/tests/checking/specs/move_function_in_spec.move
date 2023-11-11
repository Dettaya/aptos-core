// issue 10883
module 0x42::move_function_in_spec {
    struct TypeInfo has copy, drop, store {
        account_address: address,
    }
    public native fun type_of<T>(): TypeInfo;

    fun foo<T>() {
        let type_info = type_of<T>();
        let account_address = type_info.account_address;
        spec {
            assert account_address == type_of<T>().account_address;
        };
    }
}

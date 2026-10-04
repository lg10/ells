use std::path::PathBuf;

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: keycheck <path>"));
    let pass_arg = std::env::args().nth(2);
    let pass = pass_arg.as_deref().filter(|s| !s.is_empty());
    match russh::keys::load_secret_key(&path, pass) {
        Ok(_) => println!("OK: 密钥加载成功"),
        Err(err) => println!("ERR: {err:?}"),
    }
}

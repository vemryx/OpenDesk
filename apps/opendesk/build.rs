//! No Windows, embute no executável o ícone oficial e os dados que o sistema
//! mostra em Propriedades e no Gerenciador de Tarefas.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/icon.ico")
        .set("ProductName", "OpenDesk")
        .set("FileDescription", "OpenDesk")
        .set("LegalCopyright", "Código aberto, licença Apache-2.0");
    // Sem o compilador de recursos o app continua funcionando, só sem o ícone
    // no arquivo; a janela usa o ícone embutido de qualquer forma.
    if let Err(err) = resource.compile() {
        println!("cargo:warning=ícone do executável não embutido: {err}");
    }
}

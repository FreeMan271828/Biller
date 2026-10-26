//! 本地 MITM 根证书：为「完全无感捕获日常浏览器流量」做准备。
//!
//! 分两步走：
//! 1. **这里**：生成/复用本地 CA（`rcgen`），给出安装与删除信任的命令；
//! 2. 代理服务器（`capture_proxy.rs`，下一阶段）：用这份 CA 动态签发叶子证书、
//!    拦截 `https://cbsh.ebank.cmbchina.com` 的请求。
//!
//! 文件都在 `%USERPROFILE%\.biller\proxy\`：
//! - `ca.pem`：根证书，要装进「受信任的根证书颁发机构」
//! - `ca.key`：**私钥，必须保密**（拿到它能解密走该代理的全部 HTTPS 流量）

use std::path::PathBuf;

use anyhow::{Context, Result};

/// CA 的显示名（装进证书库后按它识别/删除）。
pub const CA_COMMON_NAME: &str = "biller local MITM";

/// 证书目录：`%USERPROFILE%\.biller\proxy`。
pub fn ca_dir() -> PathBuf {
    crate::common::db::default_sqlite_path()
        .parent()
        .map(|parent| parent.join("proxy"))
        .unwrap_or_else(|| PathBuf::from("proxy"))
}

pub fn ca_cert_path() -> PathBuf {
    ca_dir().join("ca.pem")
}

pub fn ca_key_path() -> PathBuf {
    ca_dir().join("ca.key")
}

/// 一份可用的本地 CA。
#[derive(Debug, Clone)]
pub struct LocalCa {
    pub cert_pem: String,
    pub key_pem: String,
    /// 这次是新建的还是复用的（提示语用）。
    pub created: bool,
}

/// 确保 CA 存在：有就复用，没有就生成。
pub fn ensure_ca() -> Result<LocalCa> {
    let cert_path = ca_cert_path();
    let key_path = ca_key_path();

    if cert_path.is_file() && key_path.is_file() {
        let cert_pem = std::fs::read_to_string(&cert_path)
            .with_context(|| format!("读取 {} 失败", cert_path.display()))?;
        let key_pem = std::fs::read_to_string(&key_path)
            .with_context(|| format!("读取 {} 失败", key_path.display()))?;

        return Ok(LocalCa {
            cert_pem,
            key_pem,
            created: false,
        });
    }

    generate()
}

/// 重新生成一对 CA（会覆盖旧的；旧证书如果还装在系统里请先删掉信任）。
pub fn generate() -> Result<LocalCa> {
    let directory = ca_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("创建目录 {} 失败", directory.display()))?;

    let key_pair = rcgen::KeyPair::generate().context("生成 CA 私钥失败")?;

    let mut params = rcgen::CertificateParams::new(vec![CA_COMMON_NAME.to_string()])
        .context("构造 CA 参数失败")?;
    // 必须标明自己是 CA，否则浏览器不会用它的签名链
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, CA_COMMON_NAME);
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "biller");
    // 十年，够用；过期后重新生成并重装信任
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let certificate = params.self_signed(&key_pair).context("签发 CA 证书失败")?;

    let cert_pem = certificate.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = ca_cert_path();
    let key_path = ca_key_path();
    std::fs::write(&cert_path, &cert_pem)
        .with_context(|| format!("写入 {} 失败", cert_path.display()))?;
    std::fs::write(&key_path, &key_pem)
        .with_context(|| format!("写入 {} 失败", key_path.display()))?;

    Ok(LocalCa {
        cert_pem,
        key_pem,
        created: true,
    })
}

/// 安装信任的命令（当前用户，不需要管理员）。
pub fn install_command() -> String {
    format!(
        "certutil -addstore -user Root \"{}\"",
        ca_cert_path().display()
    )
}

/// 删除信任的命令。
pub fn remove_command() -> String {
    format!("certutil -delstore -user Root \"{CA_COMMON_NAME}\"")
}

/// 状态描述：文件在不在、装上信任该跑什么。
pub fn status_lines() -> Result<Vec<String>> {
    let cert = ca_cert_path();
    let key = ca_key_path();

    let mut lines = Vec::new();
    lines.push(format!("证书目录：{}", ca_dir().display()));
    lines.push(format!(
        "ca.pem  ：{}",
        if cert.is_file() {
            format!("已存在（{} 字节）", std::fs::metadata(&cert)?.len())
        } else {
            "缺失".to_string()
        }
    ));
    lines.push(format!(
        "ca.key  ：{}（务必要保密）",
        if key.is_file() { "已存在" } else { "缺失" }
    ));
    lines.push(format!(
        "安装信任：{}",
        if cert.is_file() {
            install_command()
        } else {
            "先运行 `biller proxy init` 生成证书".to_string()
        }
    ));
    lines.push(format!("删除信任：{}", remove_command()));

    Ok(lines)
}

/// 测试串行锁：`proxy_ca` 与 `capture_proxy` 的用例都会读写**同一个真实 CA 目录**，
/// 并行跑会互相删除文件导致偶发红。凡是要碰真实证书目录的测试都先拿这把锁。
#[cfg(test)]
pub(crate) fn test_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_paths_live_under_the_biller_directory() {
        let directory = ca_dir();
        assert_eq!(directory.file_name().unwrap(), "proxy");
        assert!(
            ca_cert_path().ends_with("proxy/ca.pem") || ca_cert_path().ends_with("proxy\\ca.pem")
        );
        assert!(
            ca_key_path().ends_with("proxy/ca.key") || ca_key_path().ends_with("proxy\\ca.key")
        );
    }

    #[test]
    fn commands_target_the_current_user_store() {
        // 必须带 -user：不带就得管理员权限
        assert!(install_command().contains("-addstore -user Root"));
        assert!(install_command().contains("ca.pem"));
        assert!(remove_command().contains("-delstore -user Root"));
        assert!(remove_command().contains(CA_COMMON_NAME));
    }

    #[test]
    fn generates_a_ca_with_usable_pem() {
        // 会写真实目录：与 capture_proxy 的用例串行，避免互相删除
        let _guard = test_serial_lock();

        // 生成一次（写到真实目录，随后清理），验证 PEM 结构
        let before = ca_cert_path().is_file();
        let ca = generate().expect("应能生成 CA");

        assert!(ca.created);
        assert!(ca.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(ca.cert_pem.contains("-----END CERTIFICATE-----"));
        assert!(
            ca.key_pem.contains("PRIVATE KEY"),
            "私钥 PEM 应包含 PRIVATE KEY 标记"
        );
        assert!(ca_cert_path().is_file());
        assert!(ca_key_path().is_file());

        // 再调一次 ensure 应该复用而不是重新生成
        let again = ensure_ca().expect("应能复用 CA");
        assert!(!again.created, "第二次应复用已有的 CA");
        assert_eq!(again.cert_pem, ca.cert_pem);

        // 清理（测试不留下证书文件；如果你本来就有，就别删）
        if !before {
            let _ = std::fs::remove_file(ca_cert_path());
            let _ = std::fs::remove_file(ca_key_path());
        }
    }
}

//! 银行同步的命令行入口。
//!
//! TUI 里的「同步今日/本周」两个按钮走的是同一套业务层，这里先给一条
//! 可以在终端直接跑的路径——也方便排查 CDP / 登录会话的问题。

use anyhow::{Context, Result, bail};

use crate::bank::client::BankAccount;
use crate::bank::model::BankTransaction;
use crate::bank::service::{BankService, DEFAULT_BOOK, DEFAULT_CATEGORY, Suggestion, SyncKind};
use crate::bank::sync;

/// `sync` 的子命令。
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SyncCmd {
    /// 检查浏览器调试通道、招行页面与 ClientNo
    Probe {
        /// 调试端口
        #[arg(long, default_value_t = 9222)]
        port: u16,
    },
    /// 起 MITM 代理并阻塞等待你日常浏览器里的一次「查流水」请求
    /// （需先 `sync ca-init` 并安装根证书信任）
    ProxyCapture {
        #[arg(long, default_value_t = 9080)]
        port: u16,
        #[arg(long, default_value_t = 180)]
        timeout: u64,
        /// 自动用 --proxy-server 拉起一个走该代理的浏览器窗口
        #[arg(long)]
        browser: bool,
    },
    /// 生成（或复用）本地 MITM 根证书，并打印安装信任的命令（MITM 第一步）
    CaInit,
    /// 查看根证书状态与安装/删除命令
    CaStatus,
    /// 阻塞等待你在浏览器窗口里点一次「查流水」，然后捕获那次请求（方案 A1，免证书）
    Capture {
        #[arg(long, default_value_t = 9222)]
        port: u16,
        /// 最多等多少秒
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// 从粘贴的 cURL / 原始请求里捕获会话信息（方案 A3，免证书）
    ///
    /// 用法：在 DevTools 的 Network 里右键那条 querytrans → Copy → Copy as cURL，
    /// 然后 `Get-Clipboard | biller sync paste`
    Paste {
        /// 从文件读取（默认从标准输入）
        #[arg(long)]
        file: Option<std::path::PathBuf>,
    },
    /// 同步今日流水
    Today {
        /// 只打印，不写库
        #[arg(long)]
        dry_run: bool,
        /// 按建议直接全部导入（不逐条确认）
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value_t = 9222)]
        port: u16,
    },
    /// 同步本周流水（从本地最新账单的次日开始）
    Week {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value_t = 9222)]
        port: u16,
    },
    /// 列出/保存同步账户
    Account {
        #[command(subcommand)]
        cmd: AccountCmd,
    },
    /// 维护关键词规则
    Keyword {
        #[command(subcommand)]
        cmd: KeywordCmd,
    },
    /// 查看/清空店家记忆
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum AccountCmd {
    /// 列出已保存的账户
    List,
    /// 保存一个账户（accountUid 与 subAccountNo 从浏览器页面里取）
    Add {
        account_uid: String,
        sub_account_no: String,
        #[arg(long, default_value = "招商银行")]
        alias: String,
    },
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum KeywordCmd {
    /// 列出关键词规则（长的在前）
    List,
    /// 新增或覆盖：关键词 → 分类名
    Add { keyword: String, category: String },
    /// 删除一个关键词
    Rm { keyword: String },
    /// 预置一批常用关键词（只补缺）
    Seed,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum MemoryCmd {
    /// 列出学到的店家记忆（最近的在前）
    List,
    /// 清空全部店家记忆
    Clear,
}

pub async fn run(service: &BankService, cmd: SyncCmd) -> Result<()> {
    match cmd {
        SyncCmd::Probe { port } => probe(port).await,
        SyncCmd::ProxyCapture {
            port,
            timeout,
            browser,
        } => proxy_capture(service, port, timeout, browser).await,
        SyncCmd::CaInit => ca_init(),
        SyncCmd::CaStatus => ca_status(),
        SyncCmd::Paste { file } => paste(file).await,
        SyncCmd::Capture { port, timeout } => capture(service, port, timeout).await,
        SyncCmd::Today { dry_run, yes, port } => {
            sync_window(service, SyncKind::Today, port, dry_run, yes).await
        }
        SyncCmd::Week { dry_run, yes, port } => {
            sync_window(service, SyncKind::Week, port, dry_run, yes).await
        }
        SyncCmd::Account { cmd } => account(service, cmd).await,
        SyncCmd::Keyword { cmd } => keyword(service, cmd).await,
        SyncCmd::Memory { cmd } => memory(service, cmd).await,
    }
}

/// MITM 第一步：准备本地根证书并打印安装信任的命令。
fn ca_init() -> Result<()> {
    use crate::bank::proxy_ca;

    let ca = proxy_ca::ensure_ca()?;

    println!(
        "{}，已写入：",
        if ca.created {
            "已生成新的根证书"
        } else {
            "已复用现有根证书"
        }
    );
    println!("  {}", proxy_ca::ca_cert_path().display());
    println!("  {}（务必保密）", proxy_ca::ca_key_path().display());

    println!("\n装进「当前用户」的受信任根证书（不需要管理员）：");
    println!("  {}", proxy_ca::install_command());

    println!("\n⚠ 拿到 ca.key 就能解密走该代理的 HTTPS 流量，不要外传；");
    println!("  不用时及时删掉信任：");
    println!("  {}", proxy_ca::remove_command());

    println!("\n（代理服务器本体还在接入中，届时 `biller sync proxy-capture` 会使用它。）");
    Ok(())
}

fn ca_status() -> Result<()> {
    for line in crate::bank::proxy_ca::status_lines()? {
        println!("{line}");
    }
    Ok(())
}

/// MITM：起代理并阻塞等待日常浏览器里的那次查流水请求。
async fn proxy_capture(
    service: &BankService,
    port: u16,
    timeout_secs: u64,
    browser: bool,
) -> Result<()> {
    let port = sync::ensure_port(port)?;

    println!("准备 MITM 捕获代理（127.0.0.1:{port}）…");
    println!("前置条件：");
    println!("  1) 已跑过 `biller sync ca-init`，并把 ca.pem 装进「当前用户 → 受信任的根证书」");
    println!("  2) 把浏览器或系统代理指到 127.0.0.1:{port}");
    println!("然后在这个窗口里点一次「查询」/「账户明细」。");
    println!("正在阻塞等待…（最多 {timeout_secs} 秒）");

    if browser {
        let child = crate::bank::browser::launch_through_proxy(port)?;
        println!(
            "已启动带代理的浏览器窗口（PID {}），它已指向 127.0.0.1:{port}，",
            child.id()
        );
        println!("在那个窗口里登录招行并点一次「查询」即可。");
    }

    let captured =
        crate::bank::capture_proxy::capture(port, std::time::Duration::from_secs(timeout_secs))
            .await?;

    println!("\n✓ 捕获成功：{}", captured.describe());
    let path = crate::bank::capture::save(&captured)?;
    println!("已保存到 {}", path.display());

    if let Some(account) = captured.account() {
        service.save_account(&account, "招商银行").await?;
        println!("账户已保存：{}", account.masked());
    }

    if captured.is_usable() {
        println!("\n之后 `biller sync today` 会优先用这份会话直接重放（不用再开代理）");
    }

    Ok(())
}

/// A1：阻塞等待你点一次「查流水」，把那次请求捕获下来。
async fn capture(service: &BankService, port: u16, timeout_secs: u64) -> Result<()> {
    let port = sync::ensure_port(port)?;

    println!("正在准备浏览器（没开就自动启动一个独立窗口）…");
    let (web_socket_url, tab_url) = sync::bank_target(port).await?;
    println!("招行页面：{tab_url}");
    println!();
    println!("现在请在那个窗口里：");
    println!("  1) 登录招商银行网银（还没登录的话）");
    println!("  2) 手动点一次「查询」/「账户明细」");
    println!("正在阻塞等待这次请求…（最多 {timeout_secs} 秒）");

    let sniffed = crate::bank::browser::wait_for_transaction(
        &web_socket_url,
        std::time::Duration::from_secs(timeout_secs),
    )
    .await?;

    let captured = crate::bank::capture::CapturedRequest {
        client_no: sniffed.client_no.clone(),
        cookie: sniffed.cookie.clone(),
        account_uid: sniffed.account_uid.clone(),
        sub_account_no: sniffed.sub_account_no.clone(),
    };

    println!("\n✓ 捕获成功：{}", captured.describe());
    if let Some(body) = &sniffed.request_body {
        println!(
            "请求体（前 120 字）：{}",
            body.chars().take(120).collect::<String>()
        );
    }

    let path = crate::bank::capture::save(&captured)?;
    println!("已保存到 {}", path.display());

    if let Some(account) = captured.account() {
        service.save_account(&account, "招商银行").await?;
        println!("账户已保存：{}", account.masked());
    }

    if captured.is_usable() {
        println!("\n之后 `biller sync today` 会优先用这份会话直接重放（不用再开浏览器）");
    }

    Ok(())
}

async fn paste(file: Option<std::path::PathBuf>) -> Result<()> {
    let text = match file {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("读取 {} 失败", path.display()))?,
        None => {
            use std::io::Read;
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("读取标准输入失败")?;
            buffer
        }
    };

    let captured = crate::bank::capture::parse(&text)?;
    let path = crate::bank::capture::save(&captured)?;

    println!("已解析：{}", captured.describe());
    println!("已保存到 {}", path.display());

    if captured.is_usable() {
        println!("\n✓ 会话可用：`biller sync today` 会直接用它重放，不需要浏览器/证书");
    } else {
        println!(
            "\n△ 还缺点东西：需要 ClientNo + Cookie + accountUid/subAccountNo 三样齐全\
             \n请复制那条 querytrans 请求（含请求体）再粘一次"
        );
    }

    Ok(())
}

async fn probe(port: u16) -> Result<()> {
    let port = sync::ensure_port(port)?;
    println!("正在监听浏览器调试端口 {port}（会顺带让页面刷新一次以捕获请求）…");

    let report = sync::probe(port).await?;

    if report.launched {
        println!();
        println!("⚑ 刚才没检测到同步浏览器，已为你启动一个独立窗口。");
        println!("  请在那个窗口里打开并登录 https://cbsh.ebank.cmbchina.com");
        println!("  登录完成后，再跑一次 `biller sync probe`（或直接 `biller sync today`）。");
    }

    println!("标签页数量：{}", report.tabs);
    match &report.bank_tab {
        Some(url) => println!("招商银行页面：{url}"),
        None => println!("招商银行页面：（未打开）"),
    }
    println!("{}", report.sniffed.describe());

    if report.sniffed.is_complete() {
        println!("\n✓ ClientNo 与账户信息都拿到了，可以同步了");
    } else if report.sniffed.captured_requests > 0 {
        println!(
            "\n△ 监听到了请求，但 ClientNo 或账户信息不全。\
             \n请确认那个窗口里已登录招商银行网银，然后刷新页面再试"
        );
    } else {
        println!(
            "\n✗ 没监听到招商银行的请求。\n\
             先用 `biller sync today` 启动浏览器窗口并完成登录，再回来跑 probe"
        );
    }

    Ok(())
}

async fn sync_window(
    service: &BankService,
    kind: SyncKind,
    port: u16,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let port = sync::ensure_port(port)?;
    let today = BankService::today();

    let Some(window) = service.window(kind, today).await? else {
        println!("本周本地已经记到最新（{}），没有需要同步的日子。", today);
        return Ok(());
    };

    // 方案 A3：之前用 `sync paste` 捕获过会话就优先直接重放，不开浏览器、不要证书
    if let Some(captured) = crate::bank::capture::load()? {
        if captured.is_usable() {
            if let Some(account) = captured.account() {
                println!("用已捕获的会话直接重放（{}）", captured.describe());
                println!(
                    "同步{}流水：{} 账户 {}",
                    window.label,
                    window.describe(),
                    account.masked()
                );

                let fetched = crate::bank::capture::replay(&captured, &account, &window).await?;
                return finish(service, window, fetched, dry_run, yes).await;
            }
        }
    }

    println!("正在启动/连接浏览器并监听会话…");
    println!("（第一次会弹出一个独立窗口，请在里面登录招商银行网银；登录后它会自动刷新）");

    let session = sync::ensure_session(port).await?;
    println!("{}", session.sniffed.describe());

    let account = match session.account() {
        Some(account) => account,
        None => match sync::stored_account(service).await? {
            Some(account) => account,
            None => bail!(
                "没从页面请求里读到账户信息。\n\
                 请确认那个窗口已登录招商银行网银并刷新一次，\
                 或用 `biller sync account add <accountUid> <subAccountNo>` 兜底保存"
            ),
        },
    };

    println!(
        "同步{}流水：{} 账户 {}",
        window.label,
        window.describe(),
        account.masked()
    );

    let fetched = sync::fetch_with(&session, &account, &window).await?;
    finish(service, window, fetched, dry_run, yes).await
}

/// 共同的后半段：去重 → 预览 → 导入（浏览器路径与粘贴重放路径共用）。
async fn finish(
    service: &BankService,
    window: crate::bank::model::SyncWindow,
    fetched: Vec<BankTransaction>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let _ = &window;
    println!("接口返回 {} 条", fetched.len());

    let fresh = service.new_transactions(fetched).await?;
    if fresh.is_empty() {
        println!("没有新的流水（都已导入过）。");
        return Ok(());
    }
    println!("其中 {} 条是新的：\n", fresh.len());

    // 先算好建议，打印成表
    let mut plan = Vec::with_capacity(fresh.len());
    for txn in &fresh {
        let suggestion = service.suggest(&txn.memo).await?;
        plan.push((txn.clone(), suggestion));
    }

    print_table(&plan);

    if dry_run {
        println!("\n（--dry-run：没有写库）");
        return Ok(());
    }
    if !yes {
        println!(
            "\n确认导入请加 --yes（当前是预览模式，未写库）。\
             \nTUI 里可以逐条核对后导入。"
        );
        return Ok(());
    }

    imported(service, &plan).await
}

fn print_table(plan: &[(BankTransaction, Suggestion)]) {
    println!(
        "{:<19} {:<4} {:>11}  {:<34} {:<14} {:<16} {}",
        "时间", "方向", "金额", "店家", "账本", "分类", "建议来源"
    );
    println!("{}", "-".repeat(118));

    for (txn, suggestion) in plan {
        println!(
            "{:<19} {:<4} {:>11}  {:<34} {:<14} {:<16} {}",
            txn.date_time.format("%Y-%m-%d %H:%M:%S"),
            match txn.kind {
                crate::bill::model::BillKind::Income => "收入",
                crate::bill::model::BillKind::Expense => "支出",
            },
            crate::common::money::format_amount(txn.amount),
            truncate(&txn.memo, 34),
            truncate(&suggestion.book, 14),
            truncate(&suggestion.category, 16),
            suggestion.source.describe(),
        );
    }
}

async fn imported(service: &BankService, plan: &[(BankTransaction, Suggestion)]) -> Result<()> {
    let mut done = 0;
    let mut failed = 0;

    for (txn, suggestion) in plan {
        match service
            .confirm(txn, &suggestion.book, &suggestion.category)
            .await
        {
            Ok(bill) => {
                done += 1;
                println!(
                    "  ✓ {} {} → {} / {}",
                    bill.created_at.format("%Y-%m-%d"),
                    crate::common::money::format_amount(bill.amount),
                    bill.book.name,
                    bill.category.name
                );
            }
            Err(error) => {
                failed += 1;
                println!("  ✗ {}：{error:#}", txn.describe());
            }
        }
    }

    println!("\n已导入 {done} 条，失败 {failed} 条。");
    if done > 0 {
        println!("记忆已更新：相同店家下次会自动带出账本和分类。");
    }

    Ok(())
}

async fn account(service: &BankService, cmd: AccountCmd) -> Result<()> {
    match cmd {
        AccountCmd::List => {
            let accounts = service.stored_accounts().await?;
            if accounts.is_empty() {
                println!(
                    "还没有保存过账户。用 `biller sync account add <accountUid> <subAccountNo>` 保存。"
                );
                return Ok(());
            }

            println!("{:<22} {:<8} {}", "别名", "尾号", "默认");
            println!("{}", "-".repeat(44));
            for stored in accounts {
                println!(
                    "{:<22} {:<8} {}",
                    truncate(&stored.alias, 22),
                    stored.account.masked(),
                    if stored.is_default { "是" } else { "" }
                );
            }
            Ok(())
        }
        AccountCmd::Add {
            account_uid,
            sub_account_no,
            alias,
        } => {
            let account = BankAccount {
                account_uid,
                sub_account_no,
            };
            service.save_account(&account, &alias).await?;
            println!("已保存账户 {}（别名：{alias}）", account.masked());
            Ok(())
        }
    }
}

async fn keyword(service: &BankService, cmd: KeywordCmd) -> Result<()> {
    match cmd {
        KeywordCmd::List => {
            let rules = service.keywords().await?;
            if rules.is_empty() {
                println!("还没有关键词规则。用 `biller sync keyword seed` 预置一批。");
                return Ok(());
            }

            println!("{:<18} {}", "关键词", "分类");
            println!("{}", "-".repeat(34));
            for rule in rules {
                println!("{:<18} {}", truncate(&rule.keyword, 18), rule.category_name);
            }
            Ok(())
        }
        KeywordCmd::Add { keyword, category } => {
            let category_id = resolve_category(service, &category).await?;
            service.set_keyword(&keyword, category_id).await?;
            println!("已设置：{keyword} → {category}");
            Ok(())
        }
        KeywordCmd::Rm { keyword } => {
            let removed = service.remove_keyword(&keyword).await?;
            if removed == 0 {
                println!("没有找到关键词「{keyword}」");
            } else {
                println!("已删除关键词「{keyword}」");
            }
            Ok(())
        }
        KeywordCmd::Seed => {
            let inserted = service.seed_default_keywords().await?;
            println!("预置完成，新插入 {inserted} 条（已存在的规则不会被覆盖）");
            Ok(())
        }
    }
}

/// 把分类名解析成 id：优先子分类，其次根分类。
async fn resolve_category(
    service: &BankService,
    name: &str,
) -> Result<crate::common::types::CategoryId> {
    let nodes = service.category_tree().await?;

    let mut root = None;
    for node in nodes {
        if node.category.name != name {
            continue;
        }
        // depth > 0 说明是子分类：更具体，优先用
        if node.depth > 0 {
            return Ok(node.category.id);
        }
        root = Some(node.category.id);
    }

    match root {
        Some(id) => Ok(id),
        None => bail!(
            "没有名为「{name}」的分类（关键词规则的目标必须是已存在的分类，\
             可用 `biller category list` 查看）"
        ),
    }
}

async fn memory(service: &BankService, cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::List => {
            let entries = service.memory_list().await?;
            if entries.is_empty() {
                println!("还没有学到任何店家记忆。");
                return Ok(());
            }

            println!("{:<40} {:<14} {:<16} {}", "店家", "账本", "分类", "命中");
            println!("{}", "-".repeat(82));
            for entry in entries {
                println!(
                    "{:<40} {:<14} {:<16} {}",
                    truncate(&entry.memo_key, 40),
                    truncate(&entry.book_name, 14),
                    truncate(&entry.category_name, 16),
                    entry.hits
                );
            }
            Ok(())
        }
        MemoryCmd::Clear => {
            let removed = service.clear_memory().await?;
            println!("已清空 {removed} 条店家记忆");
            Ok(())
        }
    }
}

fn truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }

    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 默认账本/分类名，供提示语用。
pub fn default_names() -> (&'static str, &'static str) {
    (DEFAULT_BOOK, DEFAULT_CATEGORY)
}

/// 打印一条同步结果（TUI 复用它做提示）。
pub fn describe_transaction(transaction: &BankTransaction) -> String {
    transaction.describe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_wide_text_by_characters() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 中文按字符算，不按字节
        assert_eq!(truncate("一二三四五", 3), "一二…");
        assert_eq!(truncate("一二三", 3), "一二三");
    }

    #[test]
    fn default_names_are_stable() {
        assert_eq!(default_names(), ("默认", "默认"));
    }
}

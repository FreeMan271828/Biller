use std::path::Path;

/// 识别图片上的文字
/// 
use ocr_rs::OcrEngine;

pub fn parse_image(image: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // 创建 OCR 引擎（使用默认配置）
    let engine = OcrEngine::new(
        "models/PP-OCRv5_mobile_det.mnn",
        "models/PP-OCRv5_mobile_rec.mnn",
        "models/ppocr_keys_v5.txt",
        None,
    )?;
    
    // 加载图像
    let image = image::open(image)?;
    
    // 一次调用完成检测和识别
    let results = engine.recognize(&image)?;
    
    let texts:Vec<String> = results
        .iter()
        .map(|item| item.text
            .trim_matches('"')
            .to_string()
        )
        .collect();
    Ok(texts)
}
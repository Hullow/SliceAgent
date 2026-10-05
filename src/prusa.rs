use crate::pack::Plate;
use std::{fs::{File, OpenOptions}, io::Write, path::{Path, PathBuf}, process::Command};
use zip::{ZipWriter, write::SimpleFileOptions};
use std::collections::BTreeMap;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub printer: String,
    pub printer_profile: String,
    pub print_profile: String,
    pub filament_profile: String,
    pub ini_path: Option<String>,
    pub bed_width: f64,
    pub bed_depth: f64,
    pub gap: f64,
    pub location: String,
    pub settings_json: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SliceMetrics {
    pub minutes: Option<f64>,
    pub grams: Option<f64>,
    pub metres: Option<f64>,
}

pub fn find_slicer() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PRUSA_SLICER") {
        if Path::new(&path).exists() { return Some(PathBuf::from(path)); }
    }
    let mac = PathBuf::from("/Applications/Original Prusa Drivers/PrusaSlicer.app/Contents/MacOS/PrusaSlicer");
    if mac.exists() { return Some(mac); }
    for path in ["/usr/bin/prusa-slicer", "/usr/local/bin/prusa-slicer", "/opt/homebrew/bin/prusa-slicer"] {
        if Path::new(path).exists() { return Some(PathBuf::from(path)); }
    }
    None
}

pub fn prepare_preset(slicer:&Path,preset:&Preset,dir:&Path)->Result<Preset,String> {
    let Some(settings)=preset.settings_json.as_deref() else {return Ok(preset.clone())};
    let values:BTreeMap<String,String>=serde_json::from_str(settings).map_err(|e|e.to_string())?;
    if values.is_empty(){return Ok(preset.clone());}
    let base=dir.join("preset-base.ini");
    let edited=dir.join("preset-edited.ini");
    let mut cmd=Command::new(slicer);
    if let Some(ini)=&preset.ini_path {cmd.arg("--load").arg(ini);}
    else {cmd.arg("--printer-profile").arg(&preset.printer_profile).arg("--print-profile").arg(&preset.print_profile).arg("--material-profile").arg(&preset.filament_profile);}
    let output=cmd.arg("--save").arg(&base).output().map_err(|e|e.to_string())?;
    if !output.status.success()||!base.exists(){return Err(format!("Could not prepare preset: {}",String::from_utf8_lossy(&output.stderr)));}
    let source=std::fs::read_to_string(&base).map_err(|e|e.to_string())?;
    std::fs::write(&edited,apply_overrides(&source,&values)).map_err(|e|e.to_string())?;
    let mut result=preset.clone();result.ini_path=Some(edited.to_string_lossy().to_string());Ok(result)
}

fn apply_overrides(source:&str,values:&BTreeMap<String,String>)->String {
    let mut lines:Vec<String>=source.lines().map(str::to_string).collect();
    for (key,value) in values {
        let mut found=false;
        for line in &mut lines {
            if line.split_once('=').is_some_and(|(left,_)|left.trim()==key) { *line=format!("{key} = {value}");found=true; }
        }
        if !found {lines.push(format!("{key} = {value}"));}
    }
    format!("{}\n",lines.join("\n"))
}

pub fn write_plate_3mf(plate: &Plate, path: &Path) -> Result<(), String> {
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    archive.start_file("[Content_Types].xml", options).map_err(|e| e.to_string())?;
    archive.write_all(br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/></Types>"#).map_err(|e| e.to_string())?;
    archive.start_file("_rels/.rels", options).map_err(|e| e.to_string())?;
    archive.write_all(br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>"#).map_err(|e| e.to_string())?;
    archive.start_file("3D/3dmodel.model", options).map_err(|e| e.to_string())?;
    let mut model = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><model unit=\"millimeter\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\"><resources>");
    for (index, placement) in plate.placements.iter().enumerate() {
        let mesh = &placement.part.oriented.mesh;
        model.push_str(&format!("<object id=\"{}\" type=\"model\" name=\"{}\"><mesh><vertices>", index+1, xml_escape(&placement.part.name)));
        for v in &mesh.vertices { model.push_str(&format!("<vertex x=\"{:.5}\" y=\"{:.5}\" z=\"{:.5}\"/>", v[0],v[1],v[2])); }
        model.push_str("</vertices><triangles>");
        for f in &mesh.faces { model.push_str(&format!("<triangle v1=\"{}\" v2=\"{}\" v3=\"{}\"/>",f[0],f[1],f[2])); }
        model.push_str("</triangles></mesh></object>");
    }
    model.push_str("</resources><build>");
    for (index, placement) in plate.placements.iter().enumerate() {
        model.push_str(&format!("<item objectid=\"{}\" transform=\"1 0 0 0 1 0 0 0 1 {:.5} {:.5} 0\"/>", index+1, placement.x, placement.y));
    }
    model.push_str("</build></model>");
    archive.write_all(model.as_bytes()).map_err(|e| e.to_string())?;
    archive.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn command(slicer: &Path, preset: &Preset, input: &Path, output: &Path, action: &str, binary: bool) -> Result<(), String> {
    let mut cmd = Command::new(slicer);
    cmd.arg("--dont-arrange").arg("--ensure-on-bed");
    if let Some(ini) = &preset.ini_path { cmd.arg("--load").arg(ini); }
    else {
        cmd.arg("--printer-profile").arg(&preset.printer_profile)
            .arg("--print-profile").arg(&preset.print_profile)
            .arg("--material-profile").arg(&preset.filament_profile);
    }
    cmd.arg(if binary { "--binary-gcode" } else { "--no-binary-gcode" });
    cmd.arg(action).arg("--output").arg(output).arg(input);
    let result = cmd.output().map_err(|e| format!("Cannot start PrusaSlicer: {e}"))?;
    if !result.status.success() || !output.exists() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        return Err(format!("PrusaSlicer failed ({}): {} {}", result.status, stderr.trim(), stdout.trim()).chars().take(2500).collect());
    }
    Ok(())
}

pub fn slice_candidate(slicer: &Path, preset: &Preset, input: &Path, gcode: &Path) -> Result<SliceMetrics, String> {
    command(slicer, preset, input, gcode, "--export-gcode", false)?;
    let text = std::fs::read_to_string(gcode).map_err(|e| e.to_string())?;
    Ok(parse_metrics(&text))
}

pub fn export_final(slicer: &Path, preset: &Preset, input: &Path, project: &Path, bgcode: &Path) -> Result<(), String> {
    command(slicer, preset, input, project, "--export-3mf", false)?;
    let config_path = project.with_extension("ini");
    if let Some(ini) = &preset.ini_path {
        std::fs::copy(ini, &config_path).map_err(|e| e.to_string())?;
    } else {
        let result = Command::new(slicer)
            .arg("--printer-profile").arg(&preset.printer_profile)
            .arg("--print-profile").arg(&preset.print_profile)
            .arg("--material-profile").arg(&preset.filament_profile)
            .arg("--save").arg(&config_path)
            .output().map_err(|e| e.to_string())?;
        if !result.status.success() || !config_path.exists() { return Err(format!("Could not save combined slicer preset: {}", String::from_utf8_lossy(&result.stderr))); }
    }
    embed_config(project, &config_path)?;
    command(slicer, preset, input, bgcode, "--export-gcode", true)
}

fn embed_config(project: &Path, config: &Path) -> Result<(), String> {
    let ini = std::fs::read_to_string(config).map_err(|e| e.to_string())?;
    let mut contents = String::from("; generated by SliceAgent\n");
    for line in ini.lines() { contents.push_str("; "); contents.push_str(line); contents.push('\n'); }
    let file = OpenOptions::new().read(true).write(true).open(project).map_err(|e| e.to_string())?;
    let mut archive = ZipWriter::new_append(file).map_err(|e| e.to_string())?;
    archive.start_file("Metadata/Slic3r_PE.config", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).map_err(|e| e.to_string())?;
    archive.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
    archive.finish().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn parse_metrics(text: &str) -> SliceMetrics {
    let mut result = SliceMetrics::default();
    for line in text.lines().rev() {
        let line = line.trim().trim_start_matches(';').trim();
        if let Some(value) = line.strip_prefix("filament used [g] =") {
            result.grams = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("filament used [mm] =") {
            result.metres = value.trim().parse::<f64>().ok().map(|mm| mm / 1000.0);
        } else if let Some(value) = line.strip_prefix("estimated printing time (normal mode) =") {
            result.minutes = parse_duration(value.trim());
        }
    }
    result
}

fn parse_duration(text: &str) -> Option<f64> {
    let mut minutes = 0.0;
    let mut found = false;
    for chunk in text.split_whitespace() {
        if chunk.len() < 2 { continue; }
        let (num, unit) = chunk.split_at(chunk.len()-1);
        let n: f64 = num.parse().ok()?;
        match unit {
            "d" => minutes += n*1440.0,
            "h" => minutes += n*60.0,
            "m" => minutes += n,
            "s" => minutes += n/60.0,
            _ => continue,
        }
        found = true;
    }
    found.then_some(minutes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preset_overrides_replace_existing_values() {
        let overrides=BTreeMap::from([("fill_density".into(),"20%".into()),("brim_width".into(),"3".into())]);
        assert_eq!(apply_overrides("layer_height = 0.2\nfill_density = 15%\n",&overrides),"layer_height = 0.2\nfill_density = 20%\nbrim_width = 3\n");
    }
    #[test]
    fn parses_prusa_summary() {
        let parsed = parse_metrics("; filament used [mm] = 1234.5\n; filament used [g] = 3.67\n; estimated printing time (normal mode) = 1h 2m 30s\n");
        assert_eq!(parsed.grams, Some(3.67));
        assert_eq!(parsed.metres, Some(1.2345));
        assert_eq!(parsed.minutes, Some(62.5));
    }
}

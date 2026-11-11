//! English for the app's own messages. The Japanese text is the key, so a message without an
//! entry simply stays Japanese. The settings page carries its own text.

use crate::config;

const ENGLISH: &[(&str, &str)] = &[
    ("翻訳中…", "Translating…"),
    ("翻訳する文字が見つかりません", "No text to translate"),
    ("エラー: ", "Error: "),
    ("設定", "Settings"),
    ("履歴", "History"),
    ("終了", "Exit"),
    ("ダブルクリックで設定を開きます", "Double-click for settings"),
    ("ホットキーを登録できません。別のアプリが使っています。設定で変更してください。", "A hotkey is taken by another app. Change it in the settings."),
    ("ライセンスに同意するまで翻訳できません", "Translation needs the license accepted first"),
    ("モデルがありません。トレイのアイコンから設定を開いてダウンロードしてください", "No model yet. Open the settings from the tray icon and download one."),
    ("モデルを読み込めません。GPU メモリの不足か、ファイルの破損が考えられます", "The model cannot be loaded. GPU memory may be short, or the file damaged."),
    ("翻訳エンジンに接続できません: ", "Cannot reach the translation engine: "),
    ("翻訳エンジンが止まりました", "The translation engine stopped"),
    ("翻訳エンジンを起動できません: ", "Cannot start the translation engine: "),
    ("翻訳エンジンが見つかりません", "The translation engine is missing"),
    ("このモデルは翻訳に使えません", "This model cannot translate"),
    ("モデルの一覧を取得できません", "Cannot get the model list"),
    ("モデルの情報が見つかりません", "Model details not found"),
    ("ダウンロードに失敗しました", "The download failed"),
    ("ダウンロードしたファイルが壊れています", "The downloaded file is damaged"),
    ("このモデルはダウンロードできません", "This model cannot be downloaded"),
    ("ハッシュを計算できません", "Cannot check the file"),
    ("そのキーは使えません（別の操作や別のアプリが使っています）", "That key is taken, by another action or another app"),
    ("削除できません: ", "Cannot delete: "),
    ("Sonoba の設定", "Sonoba settings"),
    ("モデルの保存先", "Model folder"),
    ("設定画面を開けません。WebView2 ランタイムが必要です。", "The settings window cannot open. It needs the WebView2 Runtime."),
    ("訳文をコピーしました", "Translation copied"),
    ("コピーする訳文がありません", "No translation to copy"),
    ("クリップボードが使えません", "The clipboard is busy"),
    ("一時停止中", "Paused"),
    ("一時停止", "Pause"),
    ("Shift+ドラッグかクリックで範囲を追加、範囲の中を右クリックで削除", "Shift+drag or click adds a region, right-click inside one removes it"),
    ("翻訳元", "From"),
    ("翻訳先", "Into"),
    ("リアルタイム", "Live"),
    ("オン", "On"),
    ("オフ", "Off"),
    ("自動", "Auto"),
    ("日本語も翻訳", "Japanese too"),
    ("韓国語", "Korean"),
    ("日本語", "Japanese"),
    ("やさしい日本語", "Easy Japanese"),
    ("英語", "English"),
    ("中国語（簡体字）", "Chinese (Simplified)"),
    ("中国語（繁体字）", "Chinese (Traditional)"),
    ("Sonoba 共有用", "Sonoba for sharing"),
];

/// The message in the language of the interface. A known beginning is enough, for messages that
/// carry a detail on the end.
pub fn t(text: &str) -> String {
    if config::get().ui_lang != "en" {
        return text.to_string();
    }
    if let Some((_, en)) = ENGLISH.iter().find(|(ja, _)| *ja == text) {
        return en.to_string();
    }
    match ENGLISH.iter().find_map(|(ja, en)| text.strip_prefix(ja).map(|rest| (en, rest))) {
        Some((en, rest)) => format!("{en}{rest}"),
        None => text.to_string(),
    }
}

// aosc-exec-guard 的图形弹框（Qt Quick + Kirigami 的观感）。
//
// guard 在 KDE 会话里优先用它（见 src/prompt.rs 的 dialog_tools()）：
//   <Qt6 的 qml 运行时> dialog.qml --title <标题> --text <正文> \
//       --checkbox <复选文字> --ok <运行按钮> --cancel <不运行按钮>
//
// 结果用退出码传回（0/1/2 是 qml 运行时自己的码：正常退出/加载出错，所以避开）：
//   10 = 运行（这次）   12 = 运行，并且记住   11 = 不运行（这次）
// 其它退出码（比如 QML 加载失败）= 没有答案，guard 会去试下一个弹框工具。
//
// 文案全部由 guard 传进来：翻译在 guard 的 locales/*.yml 里，这个文件除了下面
// 的默认值不留字符串（也就没有第二份 i18n）。
//
// `--test-answer=run|remember|decline` 给测试用：不弹窗，直接返回对应退出码。

import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

Controls.ApplicationWindow {
    id: app

    // 与 guard 的约定（见 src/prompt.rs 里的 QML_* 常量）
    readonly property int answerRun: 10
    readonly property int answerDecline: 11
    readonly property int answerRunRemember: 12

    readonly property int pad: Kirigami.Units.largeSpacing * 2

    /// 取一个 `--名字 值` 形式的参数。
    function arg(name, fallback) {
        const args = Qt.application.arguments;
        const index = args.indexOf(name);
        return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
    }

    /// 运行；勾了“不再询问”就用另一个退出码，让 guard 把选择记下来。
    function accept(remembered) {
        Qt.exit(remembered ? answerRunRemember : answerRun);
    }

    title: arg("--title", "aosc-exec-guard")
    // ApplicationWindow 不显式写 visible 就根本不出窗口（offscreen 平台看不出来，
    // 只有真会话里才暴露），所以这行不能省。
    visible: true
    // 普通对话框窗口：KWin 会画 Breeze 标题栏（和 kdialog 一样），不需要
    // 透明窗口/合成器，也不挡整个屏幕。
    flags: Qt.Dialog
    width: Math.round(Kirigami.Units.gridUnit * 28)
    height: column.implicitHeight + buttons.implicitHeight + pad * 3

    Column {
        id: column
        anchors {
            left: parent.left
            right: parent.right
            top: parent.top
            margins: app.pad
        }
        spacing: Kirigami.Units.largeSpacing

        Controls.Label {
            id: text
            width: column.width
            text: app.arg("--text", "")
            wrapMode: Text.WordWrap
        }

        Controls.CheckBox {
            id: remember
            width: column.width
            text: app.arg("--checkbox", "")
        }
    }

    Row {
        id: buttons
        anchors {
            right: parent.right
            bottom: parent.bottom
            margins: app.pad
        }
        spacing: Kirigami.Units.largeSpacing

        Controls.Button {
            text: app.arg("--ok", "OK")
            highlighted: true
            icon.name: "media-playback-start"
            onClicked: app.accept(remember.checked)
        }

        Controls.Button {
            text: app.arg("--cancel", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: Qt.exit(app.answerDecline)
        }
    }

    // Esc / 关窗 = 不运行（和“不运行”按钮一个意思）。
    Shortcut {
        sequence: "Escape"
        onActivated: Qt.exit(app.answerDecline)
    }
    onClosing: (close) => {
        close.accepted = false;
        Qt.exit(app.answerDecline);
    }
    // 回车 = 运行：图形框的默认按钮和 zenity/kdialog 一样是“运行”。
    Shortcut {
        sequence: "Return"
        onActivated: app.accept(remember.checked)
    }

    Component.onCompleted: {
        const test = arg("--test-answer", "");
        let code = 0;
        if (test === "run") {
            code = answerRun;
        } else if (test === "remember") {
            code = answerRunRemember;
        } else if (test === "decline") {
            code = answerDecline;
        }
        if (code !== 0) {
            // 事件循环还没转起来，直接 Qt.exit() 不管用，得推迟一下。
            Qt.callLater(function () { Qt.exit(code); });
        }
    }
}

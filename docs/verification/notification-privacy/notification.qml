import QtQuick
import Quickshell
import "./final-plugin" as Plugin
ShellRoot {
  Plugin.Service { id: service; shell: QtObject { property bool standalone: true; function summon(a,b) { console.info("OPEN_REVIEW", a, b) } } }
  Timer { interval: 1500; running: true; onTriggered: {
    service.settings = {request_popups: Quickshell.env("PRIVACY_POPUPS") === "true", popup_desktop_notifications: Quickshell.env("PRIVACY_POPUPS") === "true"};
    service.requestDismissalsReady = true; service.dndRead = true; service.persistedDnd = false;
    service.attention = [
      {kind:"approval", ref:"att_privacy_a", computer_id:"fixture", label:"PRIVATE_LABEL_CANARY", summary:"PRIVATE_SUMMARY_CANARY", effect:"send"},
      {kind:"question", ref:"att_privacy_q", computer_id:"fixture", label:"PRIVATE_LABEL_CANARY", summary:"PRIVATE_QUESTION_CANARY", options:["PRIVATE_OPTION_CANARY","OTHER_PRIVATE_CANARY"]},
      {kind:"login", ref:"att_privacy_l", computer_id:"fixture", label:"PRIVATE_LABEL_CANARY", login:{sites:[{site:"private-site-canary.example"}], assistance:{revision:1,notification:"normal"}}}
    ];
    service.syncRequestNotices();
    service.queueNotice({title:"PRIVATE_TITLE_CANARY",body:"PRIVATE_BODY_CANARY"});
  } }
  Timer { interval: 4500; running: true; onTriggered: Qt.quit() }
}

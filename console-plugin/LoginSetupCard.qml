import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// Once the console is set up, until it is answered: may agents use your logins? What ibara does
// (copies one site's login from your browser to an agent's computer, only when you allow it),
// that the browser will say "Managed by your organization", and the browsers and profiles found
// here, with a choice when there are several. Turn On makes this the computer logins come from;
// Not Now leaves sharing off (the Logins page turns it on later).
Toast {
  id: root
  property var host: null
  property var service: null
  readonly property Tokens tokens: Tokens {}
  readonly property var settings: service ? service.loginSettings : null
  readonly property var browsers: settings ? settings.browsers : []
  readonly property var pick: service ? service.loginPick : null
  // One entry per browser and profile ibara can share from.
  readonly property var choices: {
    var out = []
    for (var i = 0; i < browsers.length; i++) for (var j = 0; j < browsers[i].profiles.length; j++)
      if (browsers[i].supported) out.push({ id: browsers[i].browser + "\n" + browsers[i].profiles[j], browser: browsers[i].browser, profile: browsers[i].profiles[j],
        label: browsers[i].name + " · " + browsers[i].profiles[j] })
    return out
  }
  readonly property string found: browsers.length
    ? "Found here: " + browsers.map(function(b) { return b.name + (b.supported ? (b.profiles.length ? " (" + b.profiles.join(", ") + ")" : " (no profile yet)") : " (" + (b.reason || "can't share") + ")") }).join("; ") + "."
    : "ibara found no Chromium, Google Chrome or Brave here, so it has no logins to share."
  readonly property bool working: !!service && !!service.busy["login-on"]
  readonly property string pickLabel: pick ? StatusModel.loginBrowserName(settings, pick.browser) + " · " + pick.profile : ""

  edge: Color.accent
  dismissable: false
  firstControl: turnOn
  Accessible.name: "Let your agents use your logins? " + intro.text

  Copy { width: parent.width; text: "Let your agents use your logins?"; font.bold: true }
  Copy {
    id: intro
    width: parent.width
    text: "When an agent reaches a sign-in page, ibara asks you here. If you allow it, ibara copies just that site's login from your browser to the agent's computer, and the page opens signed in. Nothing is copied until you allow it, and never your passwords, history or other sites."
    font.pixelSize: Style.font.bodySmall
  }
  Copy {
    width: parent.width
    text: "Your browser will show “Managed by your organization”: that is how ibara adds its extension to it."
    dimmed: true
    font.pixelSize: Style.font.bodySmall
  }
  Copy { width: parent.width; text: root.found; color: root.browsers.length ? root.tokens.foreground : root.tokens.textTint(root.tokens.pausedColor); font.pixelSize: Style.font.bodySmall }
  Flow {
    width: parent.width
    spacing: Style.space(8)
    ActionButton {
      id: turnOn
      label: root.working ? "Turning On…" : "Turn On"
      role: "primary"
      size: "small"
      blocked: root.working || !root.pick
      disabledReason: root.working ? "ibara is turning it on." : !root.pick ? "There is no browser here to share logins from." : ""
      tooltipText: disabledReason || "Share logins from " + root.pickLabel + ", only when you allow them"
      Accessible.name: "Turn on login sharing from " + root.pickLabel
      onClicked: if (!blocked && root.host) root.host.turnOnLogins(turnOn)
    }
    // With more than one browser or profile, which one logins come from.
    ActionMenu {
      visible: root.choices.length > 1
      size: "small"
      role: "secondary"
      label: root.pickLabel + " ▾"
      accessibleName: "Share logins from: " + root.pickLabel
      tooltipText: "Choose the browser and profile logins come from"
      blocked: root.working
      items: root.choices.map(function(c) { return { id: c.id, label: c.label, selected: !!root.pick && c.browser === root.pick.browser && c.profile === root.pick.profile } })
      onTriggered: id => { var parts = id.split("\n"); root.service.chooseLoginBrowser(parts[0], parts[1]) }
    }
    ActionButton {
      label: "Not Now"
      role: "quiet"
      size: "small"
      blocked: root.working
      tooltipText: "Leave login sharing off. You can turn it on in the Logins page."
      onClicked: if (!blocked && root.service) root.service.loginNotNow()
    }
  }
}

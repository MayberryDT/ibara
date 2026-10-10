// Presentation fallback for consoles connected to an older daemon.
var help = {
  "screen_stream": "Automatic uses ibara's stream when graphics can encode it.",
  "hand_back_seconds": "Let agents continue after you stop using the screen.",
  "check_for_updates": "Check for new ibara releases.",
  "input_backend": "Choose how agents use the keyboard and mouse.",
  "name": "Name this computer for people and agents.",
  "virtual_display_size": "Set the size of the screen without a monitor.",
  "preview_seconds": "Set the time between screen pictures.",
  "auto_resume": "Let agents continue after a restart.",
  "self_repair": "Fix common screen and agent problems automatically.",
  "wake_on_network": "Let another computer wake this one.",
  "shared_clipboard": "Copy and paste between computers.",
  "ask_first": "Ask you before agents send, spend or delete.",
  "fleet_ask_first": "Follow the approval choice in Settings.",
  "agents_ask_first": "Ask you before agents send, spend or delete.",
  "notifications": "Show alerts for requests and finished tasks.",
  "approval_notifications": "Show approval buttons in desktop alerts.",
  "download_folder": "Choose where received files are saved.",
  "fleet_preview_seconds": "Set the time between fleet pictures.",
  "live_video": "Show live video on the fleet page.",
  "unattended_boot": "Start this computer without typing the disk password.",
  "lock_at_sign_in": "Lock the screen after automatic sign-in.",
  "login_sharing": "Share only the site logins you allow."
}
var details = {
  "input_backend": "Automatic uses dispatchers on Hypoland and the Cua plugin on Hyprland. Dispatchers have reduced input safety when the plugin is unavailable.",
  "ask_first": "Same as in Settings follows your console\u2019s approval choice. Turning approvals off applies to your own agents. Other people\u2019s agents still ask unless you choose Always Allow. Access rules still apply.",
  "agents_ask_first": "Turning this off lets your own agents send, spend and delete on computers that follow Settings. Other people\u2019s agents still ask. Denied access and permissions to administer or join stay in place.",
  "login_sharing": "ibara copies only a site you allow from your browser. Your browser shows \u201cManaged by your organization\u201d while sharing is on."
}

var titles = {
  "screen_stream": "Screen Stream",
  "hand_back_seconds": "Hand Back After I Stop For",
  "check_for_updates": "Check for ibara Updates",
  "input_backend": "Input Backend",
  "name": "Computer Name",
  "virtual_display_size": "Virtual Screen Size",
  "preview_seconds": "Picture Interval",
  "auto_resume": "Resume Agents After a Restart",
  "self_repair": "Repair Known Problems",
  "wake_on_network": "Wake from the Network",
  "shared_clipboard": "Shared Clipboard",
  "ask_first": "Ask Before Agents Send, Spend or Delete",
  "fleet_ask_first": "Ask Before Agents Send, Spend or Delete, as in Settings",
  "agents_ask_first": "Ask Before Agents Send, Spend or Delete",
  "notifications": "Notifications",
  "approval_notifications": "Approval Notifications",
  "download_folder": "Download Folder",
  "fleet_preview_seconds": "Fleet Picture Interval",
  "live_video": "Live Video (Preview)",
  "unattended_boot": "Start Without the Disk Password",
  "lock_at_sign_in": "Lock the Screen at Sign-In"
}

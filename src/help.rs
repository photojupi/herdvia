//! `herdvia --help` and `herdvia <command> --help`. Fixed text only, so asking for help reads no
//! data folder and reaches no agent.

/// Whether an argument asks for help.
pub fn is_flag(arg: &str) -> bool {
    arg == "--help" || arg == "-h"
}

const AGENT_USAGE: &str = if cfg!(windows) {
    "agent [install|uninstall|reload|--log]"
} else {
    "agent [install|uninstall|reload]"
};

/// What `herdvia --help`, `herdvia -h`, and `herdvia help` print.
pub fn overview() -> String {
    format!(
        "herdvia carries Herdr sessions (hc machine add --via) between your own PCs over iroh,
so a PC behind NAT is reachable without SSH or a VPN. Every PC runs the herdvia agent,
and each PC keeps a list of the peers it trusts, saved when you pair them.

usage: herdvia <command> [<arguments>]

commands:
  {AGENT_USAGE:<41}run the agent, or manage its automatic startup
  listen [<name>]                          wait for a PC to pair with this one
  connect <name> [<invite>]                pair with a waiting PC and add it to Herdr
  register <name>                          add an already paired PC to Herdr again
  peers                                    list the saved peers
  id                                       print this PC's EndpointId and fingerprint
  allow <name> <EndpointId> <direction>    save a peer by its full EndpointId
  revoke <name>                            remove a peer
  status                                   show each peer's connection and why it is down
  pending                                  list connection attempts this PC turned away
  accept <name> <fingerprint> <direction>  allow one of those attempts by its fingerprint
  doctor                                   check this PC's setup without changing anything
  --version                                print the version
  via <peer> <line>                        internal: run by hc, not by hand

Run herdvia <command> --help for a command's usage and an example.
"
    )
}

/// The help for one command, or `None` when herdvia has no command by that name.
pub fn command(name: &str) -> Option<String> {
    let text = match name {
        "agent" => {
            let log = if cfg!(windows) {
                "  --log      run the agent with its output appended to\n             %LOCALAPPDATA%\\herdvia\\herdvia.log (what agent install starts)\n"
            } else {
                ""
            };
            return Some(format!(
                "usage: herdvia {AGENT_USAGE}

The agent keeps this PC's connections to its peers. listen, connect, status, pending,
accept, and Herdr sessions need it running. herdvia agent runs it in this terminal until
Ctrl-C.

  install    start the agent at every login, and now (macOS and Windows)
  uninstall  stop the background agent and remove its automatic startup; keeps the data
  reload     have the running agent re-read the peers file and retry waiting peers now
{log}
example: herdvia agent install
"
            ));
        }
        "listen" => {
            "usage: herdvia listen [<name>]

Run this on the PC that Herdr should reach. It waits up to 5 minutes for a pairing
request from herdvia connect, shows a 6-digit code to compare with the other PC, and
asks before it lets that PC open sessions here (inbound). Without <name>, the other PC's
host name is used. Needs a running agent.

example: herdvia listen laptop
"
        }
        "connect" => {
            "usage: herdvia connect <name> [<invite>]

Run this on the PC where Herdr runs, while the other PC runs herdvia listen. Pick that
PC by the fingerprint its listen screen shows, or give the hvi1-... invite it printed
when it is on another network. After you both confirm the same 6-digit code, it saves
the PC as <name> (outbound) and adds it to Herdr with hc machine add. Needs a running
agent.

example: herdvia connect win1
"
        }
        "register" => {
            "usage: herdvia register <name>

Adds an already paired outbound peer to Herdr again (hc machine add --via ...), for when
connect paired it but the Herdr registration failed. It changes no key or permission and
does not need the agent. If hc is missing or fails, it prints the command to run by
hand.

example: herdvia register win1
"
        }
        "peers" => {
            "usage: herdvia peers

Lists the saved peers: name, permission (in, out, or both), and fingerprint. It reads
only the peers file, so it works without the agent and creates nothing. herdvia status
shows the live connections.

example: herdvia peers
"
        }
        "id" => {
            "usage: herdvia id

Prints this PC's EndpointId on the first line and its fingerprint on the second. The
EndpointId is what herdvia allow takes on another PC. Creates this PC's key if it does
not exist yet.

example: herdvia id
"
        }
        "allow" => {
            "usage: herdvia allow <name> <EndpointId> (--inbound|--outbound|--both)

Saves a peer by its full EndpointId (herdvia id on that PC prints it), replacing any
peer with that name. --inbound lets it open sessions on this PC, --outbound lets this PC
connect to it, --both does both. A running agent applies the change at once.

example: herdvia allow win1 <EndpointId from herdvia id on win1> --outbound
"
        }
        "revoke" => {
            "usage: herdvia revoke <name>

Removes the peer named <name>; a running agent closes its connections at once. It does
not remove the machine from Herdr (hc machine rm <name>) or this PC from the other PC's
peers (run herdvia revoke there too). To remove a peer an older version saved as --help
or -h, put -- before its name: herdvia revoke -- --help.

example: herdvia revoke win1
"
        }
        "status" => {
            "usage: herdvia status

Shows each peer's permission, connection state, path (direct or relay), and round-trip
time, then a line for each peer that is not connected with the reason, when known, and
when it retries. herdvia agent reload retries now. Needs a running agent.

example: herdvia status
"
        }
        "pending" => {
            "usage: herdvia pending

Lists PCs whose connection attempts this PC turned away because they may not open
sessions here: fingerprint, EndpointId, attempts, and time since the last one. The agent
keeps at most 16, each for 10 minutes after its last attempt. Needs a running agent.

example: herdvia pending
"
        }
        "accept" => {
            "usage: herdvia accept <name> <fingerprint> (--inbound|--both)

Saves the one pending attempt with this fingerprint as the peer <name>. Check the
fingerprint against herdvia id on that PC. If no attempt or more than one matches, use
herdvia allow with the full EndpointId instead. Needs a running agent.

example: herdvia accept laptop 0123-4567-89ab-cdef --inbound
"
        }
        "doctor" => {
            "usage: herdvia doctor

Checks this PC's setup without changing anything: the executable, data folder, key,
peers file, herdr-core, automatic startup, agent, relay, each peer, and pending
attempts. Prints an ok, warn, or fail line for each and exits with 1 if any check fails.

example: herdvia doctor
"
        }
        "via" => {
            "usage: herdvia via <peer> <line>

Internal: hc runs this for machines added with --via. It is not meant to be run by hand.
"
        }
        _ => return None,
    };
    Some(text.to_owned())
}

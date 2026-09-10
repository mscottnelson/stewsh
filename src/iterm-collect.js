var app = Application('iTerm2');
var rows = [];
if (!app.running()) throw Error('iTerm2 is not running; previous observations preserved');
app.windows().forEach(function(w, wi) {
    w.tabs().forEach(function(t, ti) {
        t.sessions().forEach(function(s, pi) {
            var cwd = '';
            try { cwd = String(s.variable({named:'path'}) || ''); } catch(e) {}
            rows.push({id:s.uniqueID(),name:s.name(),tty:s.tty(),cwd:cwd,
                text:s.contents(),prompt:s.isAtShellPrompt(),
                location:'Window '+(wi+1)+' / Tab '+(ti+1)+' / Pane '+(pi+1)});
        });
    });
});
JSON.stringify(rows);

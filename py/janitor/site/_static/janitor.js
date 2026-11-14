var handlers = [];

registerHandler = function(kind, cb) {
  handlers.push({'kind': kind, 'callback': cb});
};

// Close the sidebar menu on phones and open it again on wider screens.
foldOnNarrowScreens = function(menu) {
  var narrow = window.matchMedia('(max-width: 600px)');
  menu.open = !narrow.matches;
  narrow.addEventListener('change', function(e) { menu.open = !e.matches; });
};

var ws_url;
window.onload = function() {
if(location.protocol == 'http:') {
    ws_url = 'ws://' + location.hostname + '/ws/notifications';
} else if(location.protocol == 'https:') {
    ws_url = 'wss://' + location.hostname + '/ws/notifications';
} else {
    console.log('Unknown protocol: ' + location.protocol);
    ws_url = undefined;
}

const connection = new WebSocket(ws_url);

connection.onerror = (error) => {
  console.log('WebSocket error: ');
  console.log(error);
}

connection.onmessage = (e) => {
  data = JSON.parse(e.data);
  handlers.forEach(function(handler) {
    if (handler.kind == data[0]) { handler.callback(data[1]); }
  });
  console.log(data);
}
}

windowbeforeunload = function(){
    socket.close();
};

// Please keep this logic in sync with janitor/site/__init__.py:format_duration
format_duration = function(n) {
   var d = moment.duration(n, "s");
   var ret = "";
   if (d.weeks() > 0) {
      return d.weeks() + "w" + (d.days() % 7) + "d";
   }
   if (d.days() > 0) {
      return d.days() + "d" + (d.hours() % 24) + "h";
   }
   if (d.hours() > 0) {
      return d.hours() + "h" + (d.minutes() % 60) + "m";
   }
   if (d.minutes() > 0) {
      return d.minutes() + "m" + (d.seconds() % 60) + "s";
   }
   return d.seconds() + "s";
};


window.chartColors = {
   red: 'rgb(255, 99, 132)',
   orange: 'rgb(255, 159, 64)',
   yellow: 'rgb(255, 205, 86)',
   green: 'rgb(75, 192, 192)',
   blue: 'rgb(54, 162, 235)',
   purple: 'rgb(153, 102, 255)',
   grey: 'rgb(201, 203, 207)'
};

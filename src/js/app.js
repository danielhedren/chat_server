"use strict;"

        // Utility functions

        function hashCode(str) {
            return Array.from(str).reduce((s, c) => Math.imul(31, s) + c.charCodeAt(0) | 0, 0);
        }

        function uuidv4() {
            return ([1e7]+-1e3+-4e3+-8e3+-1e11).replace(/[018]/g, c => (c ^ crypto.getRandomValues(new Uint8Array(1))[0] & 15 >> c / 4).toString(16));
        }

        // WebSocket handling

        ws = new WebSocket("wss://geochat.danielhedren.com/ws/");
        ws.onopen = (e) => {
            let username = uuidv4();

            let data = { Register: { username: username, password: "password" } }
            ws.send(JSON.stringify(data));
        }

        ws.onmessage = (e) => {
            console.log(e);
            let obj = JSON.parse(e.data);

            if ((obj.hasOwnProperty("LoginResponse") && obj.LoginResponse.status) || (obj.hasOwnProperty("RegisterResponse") && obj.RegisterResponse.status)) {
                sendLocation();
            } else if (obj.hasOwnProperty("Message")) {
                displayMessage(obj.Message.username + ": " + obj.Message.msg);
            } else if (obj.hasOwnProperty("ReachResponse")) {
                displayMessage("Reach " + obj.ReachResponse.reach);
            }
        }

        // UI functions

        function sendLocation() {
            if ("geolocation" in navigator && ws.readyState === ws.OPEN) {
                navigator.geolocation.getCurrentPosition(function(position) {
                    let location = { Location: { lat: position.coords.latitude, lon: position.coords.longitude } };
                    ws.send(JSON.stringify(location));
                });
            }
        }

        function displayMessage(msg) {
            let messages = document.getElementById("messages");
            messages.innerHTML += msg + "<br>";
            messages.scrollTop = messages.scrollHeight;
        }

        function sendMessage() {
            let message_text = document.getElementById("message_text");
            let message = message_text.value;
            if (message.length == 0 || message.length > 300) return;

            let data = { SendMessage: { msg: message_text.value } };
            message_text.value = "";

            ws.send(JSON.stringify(data));
        }

        // Event setup

        document.querySelector("#message_text").addEventListener("keyup", (e) => {
            if (e.key === "Enter") {
                sendMessage();
            }
        });

        document.querySelector("#input button").addEventListener("click", sendMessage);
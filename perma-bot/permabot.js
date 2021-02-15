#!/usr/bin/env node
const delay = ms => new Promise(resolve => setTimeout(resolve, ms))

function load_config(file) {
    try {
        const data = fs.readFileSync(file, 'utf8')
        return data;
    } catch (err) {
        console.error(err);
    }
}

function setupWS(url, bot) {
    return new Promise(resolve => {
        const socket = new WebSocket(url);

        socket.onopen = (ev) => {
            console.log('ws connected')
            resolve(socket);
        }

        socket.onmessage = (ev) => {
            const event = JSON.parse(ev.data);
            if(event.type === "proposalData") {
                bot.handle_proposal(event.data);
            }
        }
    })
}

// Load config
if (process.argv.length < 5) {
    console.error(`Please use command like '${process.argv[0]} ${process.argv[1]} <clientConfig> <name> <lobbyId>' this `)
    process.exit(1);
}

const fs = require('fs');
const axios = require('axios');
const WebSocket = require('ws');

class Bot {
    constructor(configLoc, name, lobbyId) {
        // This could be better no?
        this.serverConfig = JSON.parse(load_config(configLoc) || process.exit(2)); // Load file or exit
        this.serverConfig.http_server = this.serverConfig.server.replace(/websocket$/, "api");

        this.name = name;
        this.lobbyId = lobbyId;
    }

    async setup() {
        const lobbies = await axios.get(`${this.serverConfig.http_server}/lobbies/${this.lobbyId}`).then(resp => resp.data);

        this.socket = await setupWS(this.serverConfig.server, this);

        const playerParams = {
            "token": this.serverConfig.token,
            "name": this.name
        };

        const resp = await axios.post(`${this.serverConfig.http_server}/lobbies/${this.lobbyId}/join`, playerParams).then(resp => resp.data);
        this.playerId = resp.id;

        this.socket.send(JSON.stringify({
            type: 'connect',
            lobbyId: this.lobbyId,
            token: this.serverConfig.token,
        }));

        // Check if there are any current proposals that are open
        for(let proposal of Object.values(lobbies.proposals)) {
            this.handle_proposal(proposal);
        }
    }

    async handle_proposal(proposal) {
        // Check if some player is this bot, and is still unanswered
        if(proposal.players.some(p => (p.player_id === this.playerId) && (p.status === 'Unanswered'))) {
            // Conserns me
            console.log("Accepting a game");
            await delay(200); // Fixes some bug somewhere

            await axios.post(`${this.serverConfig.http_server}/lobbies/${this.lobbyId}/proposals/${proposal.id}/accept`,
              { status: 'Accepted' },
              { headers: { 'Authorization': `Bearer ${this.serverConfig.token}` } }
            );
        }
    }
}

// Start bot
const bot = new Bot(process.argv[2], process.argv[3], process.argv[4]);
bot.setup();

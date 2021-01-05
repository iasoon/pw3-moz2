import msgpack
import subprocess
import json
import time

BIN_PATH = './target/debug/embedded'

def write_msg(f, msg):
    buf = msgpack.packb(msg, use_bin_type=True)
    f.write(len(buf).to_bytes(4, byteorder='big'))
    f.write(buf)
    f.flush()

def read_msg(f):
    length = int.from_bytes(f.read(4), byteorder='big')
    buf = f.read(length)
    return msgpack.unpackb(buf)

proc = subprocess.Popen([BIN_PATH],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE)

write_msg(proc.stdin, {
    'action': 'start_match',
    'match_id': 'mycoolmatch',
    'num_players': 2,
    'config': {
        'map_file': 'hex.json',
        'max_turns': 20,
    }
})

while True:
    # WHY IS THIS REQUIRED
    time.sleep(0.01)

    msg = read_msg(proc.stdout)
    if msg['type'] == 'player_request':
        # request data is in msg['content'].
        # This is a json-encoded binary string.
        write_msg(proc.stdin, {
            'action': 'player_response',
            'match_id': msg['match_id'],
            'player_id': msg['player_id'],
            'request_id': msg['request_id'],
            'content': json.dumps({'moves': []}).encode('utf-8'),
        })
    if msg['type'] == 'match_finished':
        print(msg['match_log'])
        break;

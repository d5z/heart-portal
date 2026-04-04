#!/usr/bin/env node

const http = require('http');
const fs = require('fs');
const path = require('path');
const url = require('url');
const crypto = require('crypto');

const PORT = process.env.PORT || 3200;
const WORKSPACE_DIR = path.join(__dirname, 'workspace');
const ACTIVITY_FILE = path.join(__dirname, '.cowork', 'activity.jsonl');
const MAX_ACTIVITY_ENTRIES = 1000;

// Ensure workspace and .cowork directories exist
if (!fs.existsSync(WORKSPACE_DIR)) {
    fs.mkdirSync(WORKSPACE_DIR, { recursive: true });
}
if (!fs.existsSync(path.dirname(ACTIVITY_FILE))) {
    fs.mkdirSync(path.dirname(ACTIVITY_FILE), { recursive: true });
}

// In-memory activity log
let activityLog = [];

// Load existing activity log
if (fs.existsSync(ACTIVITY_FILE)) {
    try {
        const data = fs.readFileSync(ACTIVITY_FILE, 'utf8');
        activityLog = data.trim().split('\n')
            .filter(line => line.trim())
            .map(line => JSON.parse(line))
            .slice(-MAX_ACTIVITY_ENTRIES);
    } catch (err) {
        console.warn('Failed to load activity log:', err.message);
    }
}

// SSE clients
const sseClients = new Set();

// Add activity entry
function addActivity(actor, action, filePath) {
    const entry = {
        time: new Date().toISOString(),
        actor,
        action,
        path: filePath
    };
    
    activityLog.push(entry);
    if (activityLog.length > MAX_ACTIVITY_ENTRIES) {
        activityLog = activityLog.slice(-MAX_ACTIVITY_ENTRIES);
    }
    
    // Save to file
    try {
        fs.appendFileSync(ACTIVITY_FILE, JSON.stringify(entry) + '\n');
    } catch (err) {
        console.warn('Failed to save activity:', err.message);
    }
    
    // Broadcast to SSE clients
    const data = JSON.stringify({ type: 'activity', data: entry });
    sseClients.forEach(client => {
        try {
            client.write(`data: ${data}\n\n`);
        } catch (err) {
            sseClients.delete(client);
        }
    });
}

// Validate and normalize path
function validatePath(requestPath) {
    if (!requestPath) return null;
    
    // Remove leading/trailing slashes and normalize
    const normalized = requestPath.replace(/^\/+|\/+$/g, '');
    const fullPath = path.resolve(WORKSPACE_DIR, normalized);
    
    // Security check: must be within workspace
    if (!fullPath.startsWith(path.resolve(WORKSPACE_DIR))) {
        return null;
    }
    
    return fullPath;
}

// Get file tree recursively
function getFileTree(dirPath, relativePath = '') {
    try {
        const items = fs.readdirSync(dirPath);
        const tree = [];
        
        for (const item of items) {
            if (item.startsWith('.')) continue; // Skip hidden files
            
            const fullPath = path.join(dirPath, item);
            const relPath = path.join(relativePath, item);
            const stat = fs.statSync(fullPath);
            
            if (stat.isDirectory()) {
                tree.push({
                    name: item,
                    path: relPath,
                    type: 'directory',
                    children: getFileTree(fullPath, relPath)
                });
            } else {
                tree.push({
                    name: item,
                    path: relPath,
                    type: 'file',
                    size: stat.size,
                    modified: stat.mtime.toISOString()
                });
            }
        }
        
        return tree.sort((a, b) => {
            if (a.type !== b.type) return a.type === 'directory' ? -1 : 1;
            return a.name.localeCompare(b.name);
        });
    } catch (err) {
        return [];
    }
}

// Watch workspace for changes
function setupFileWatcher() {
    try {
        fs.watch(WORKSPACE_DIR, { recursive: true }, (eventType, filename) => {
            if (!filename || filename.startsWith('.')) return;
            
            const fullPath = path.join(WORKSPACE_DIR, filename);
            let action = 'edit';
            
            try {
                if (!fs.existsSync(fullPath)) {
                    action = 'delete';
                } else {
                    const stat = fs.statSync(fullPath);
                    if (stat.birthtimeMs === stat.mtimeMs) {
                        action = 'create';
                    }
                }
            } catch (err) {
                action = 'delete';
            }
            
            addActivity('fs', action, filename);
            
            // Broadcast file change
            const data = JSON.stringify({
                type: 'file-changed',
                data: { path: filename, action }
            });
            sseClients.forEach(client => {
                try {
                    client.write(`data: ${data}\n\n`);
                } catch (err) {
                    sseClients.delete(client);
                }
            });
        });
    } catch (err) {
        console.warn('File watcher setup failed:', err.message);
    }
}

// Handle API requests
function handleAPI(req, res) {
    const parsedUrl = url.parse(req.url, true);
    const pathname = parsedUrl.pathname;
    const query = parsedUrl.query;
    
    // Set CORS headers
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, DELETE, OPTIONS');
    res.setHeader('Access-Control-Allow-Headers', 'Content-Type');
    
    if (req.method === 'OPTIONS') {
        res.writeHead(200);
        res.end();
        return;
    }
    
    if (pathname === '/api/files' && req.method === 'GET') {
        // Get file tree
        const tree = getFileTree(WORKSPACE_DIR);
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(tree, null, 2));
        
    } else if (pathname === '/api/file' && req.method === 'GET') {
        // Read file
        const filePath = validatePath(query.path);
        if (!filePath) {
            res.writeHead(400, { 'Content-Type': 'text/plain' });
            res.end('Invalid path');
            return;
        }
        
        try {
            const content = fs.readFileSync(filePath, 'utf8');
            
            // For raw=1, serve HTML files directly for iframe (same origin)
            if (query.raw === '1' && (filePath.endsWith('.html') || filePath.endsWith('.htm'))) {
                res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
                res.end(content);
            } else {
                res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8' });
                res.end(content);
            }
        } catch (err) {
            res.writeHead(404, { 'Content-Type': 'text/plain' });
            res.end('File not found');
        }
        
    } else if (pathname === '/api/file' && req.method === 'PUT') {
        // Write file
        const filePath = validatePath(query.path);
        if (!filePath) {
            res.writeHead(400, { 'Content-Type': 'text/plain' });
            res.end('Invalid path');
            return;
        }
        
        let body = '';
        req.on('data', chunk => body += chunk);
        req.on('end', () => {
            try {
                // Ensure directory exists
                const dir = path.dirname(filePath);
                if (!fs.existsSync(dir)) {
                    fs.mkdirSync(dir, { recursive: true });
                }
                
                const existed = fs.existsSync(filePath);
                fs.writeFileSync(filePath, body, 'utf8');
                
                addActivity('web', existed ? 'edit' : 'create', query.path);
                
                res.writeHead(200, { 'Content-Type': 'text/plain' });
                res.end('OK');
            } catch (err) {
                res.writeHead(500, { 'Content-Type': 'text/plain' });
                res.end('Write failed: ' + err.message);
            }
        });
        
    } else if (pathname === '/api/mkdir' && req.method === 'POST') {
        // Create directory
        const dirPath = validatePath(query.path);
        if (!dirPath) {
            res.writeHead(400, { 'Content-Type': 'text/plain' });
            res.end('Invalid path');
            return;
        }
        
        try {
            fs.mkdirSync(dirPath, { recursive: true });
            addActivity('web', 'create', query.path + '/');
            res.writeHead(200, { 'Content-Type': 'text/plain' });
            res.end('OK');
        } catch (err) {
            res.writeHead(500, { 'Content-Type': 'text/plain' });
            res.end('Create failed: ' + err.message);
        }
        
    } else if (pathname === '/api/file' && req.method === 'DELETE') {
        // Delete file/directory (move to trash)
        const targetPath = validatePath(query.path);
        if (!targetPath) {
            res.writeHead(400, { 'Content-Type': 'text/plain' });
            res.end('Invalid path');
            return;
        }
        
        try {
            const trashDir = path.join(WORKSPACE_DIR, '.trash');
            if (!fs.existsSync(trashDir)) {
                fs.mkdirSync(trashDir, { recursive: true });
            }
            
            const fileName = path.basename(targetPath);
            const timestamp = Date.now();
            const trashPath = path.join(trashDir, `${fileName}.${timestamp}`);
            
            fs.renameSync(targetPath, trashPath);
            addActivity('web', 'delete', query.path);
            
            res.writeHead(200, { 'Content-Type': 'text/plain' });
            res.end('OK');
        } catch (err) {
            res.writeHead(500, { 'Content-Type': 'text/plain' });
            res.end('Delete failed: ' + err.message);
        }
        
    } else if (pathname === '/api/activity' && req.method === 'GET') {
        // Get activity log
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(activityLog.slice(-100), null, 2));
        
    } else if (pathname === '/api/events' && req.method === 'GET') {
        // Server-Sent Events
        res.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            'Connection': 'keep-alive',
            'Access-Control-Allow-Origin': '*'
        });
        
        // Send initial connection event
        res.write('data: {"type":"connected"}\n\n');
        
        // Add client to SSE set
        sseClients.add(res);
        
        // Clean up on disconnect
        req.on('close', () => {
            sseClients.delete(res);
        });
        
        return; // Don't end response
        
    } else if (pathname === '/api/upload' && req.method === 'POST') {
        // File upload (simple binary body with path in query)
        const uploadPath = validatePath(query.path);
        if (!uploadPath) {
            res.writeHead(400, { 'Content-Type': 'text/plain' });
            res.end('Invalid path');
            return;
        }
        
        const chunks = [];
        req.on('data', chunk => chunks.push(chunk));
        req.on('end', () => {
            try {
                const buffer = Buffer.concat(chunks);
                
                // Ensure directory exists
                const dir = path.dirname(uploadPath);
                if (!fs.existsSync(dir)) {
                    fs.mkdirSync(dir, { recursive: true });
                }
                
                fs.writeFileSync(uploadPath, buffer);
                addActivity('web', 'upload', query.path);
                
                res.writeHead(200, { 'Content-Type': 'text/plain' });
                res.end('OK');
            } catch (err) {
                res.writeHead(500, { 'Content-Type': 'text/plain' });
                res.end('Upload failed: ' + err.message);
            }
        });
        
    } else {
        res.writeHead(404, { 'Content-Type': 'text/plain' });
        res.end('Not found');
    }
}

// Create HTTP server
const server = http.createServer((req, res) => {
    const parsedUrl = url.parse(req.url);
    
    if (parsedUrl.pathname.startsWith('/api/')) {
        handleAPI(req, res);
    } else if (parsedUrl.pathname === '/components.js') {
        // Serve components library
        try {
            const jsPath = path.join(__dirname, 'components.js');
            const js = fs.readFileSync(jsPath, 'utf8');
            res.writeHead(200, { 'Content-Type': 'application/javascript; charset=utf-8' });
            res.end(js);
        } catch (err) {
            res.writeHead(404, { 'Content-Type': 'text/plain' });
            res.end('components.js not found');
        }
    } else if (parsedUrl.pathname === '/' || parsedUrl.pathname === '/index.html') {
        // Serve main HTML file
        try {
            const htmlPath = path.join(__dirname, 'index.html');
            const html = fs.readFileSync(htmlPath, 'utf8');
            res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
            res.end(html);
        } catch (err) {
            res.writeHead(404, { 'Content-Type': 'text/plain' });
            res.end('index.html not found');
        }
    } else {
        res.writeHead(404, { 'Content-Type': 'text/plain' });
        res.end('Not found');
    }
});

// Start server
server.listen(PORT, () => {
    console.log(`🚀 Cowork Space running on http://localhost:${PORT}`);
    console.log(`📁 Workspace: ${WORKSPACE_DIR}`);
    setupFileWatcher();
});

// Graceful shutdown
process.on('SIGINT', () => {
    console.log('\n🛑 Shutting down...');
    sseClients.forEach(client => {
        try {
            client.end();
        } catch (err) {
            // Ignore
        }
    });
    server.close(() => {
        process.exit(0);
    });
});
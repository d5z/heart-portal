// Cowork Space Native Base Components
// Single-file component library with zero external dependencies

(function(global) {
    'use strict';

    // Global theme colors
    const THEME = {
        bg: '#1a1a2e',
        bgLight: '#1e1e3f',
        bgDark: '#16193d',
        text: '#e0e0e0',
        textMuted: '#a0a0a0',
        primary: '#4ec9b0',
        secondary: '#569cd6',
        danger: '#e94560',
        border: '#2a2a5e'
    };

    // Utility functions
    function createElement(tag, className, style = {}) {
        const el = document.createElement(tag);
        if (className) el.className = className;
        Object.assign(el.style, style);
        return el;
    }

    function createIcon(path, size = 16) {
        const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        svg.setAttribute('width', size);
        svg.setAttribute('height', size);
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'currentColor');
        const pathEl = document.createElementNS('http://www.w3.org/2000/svg', 'path');
        pathEl.setAttribute('d', path);
        svg.appendChild(pathEl);
        return svg;
    }

    // Common styles
    const commonStyles = `
        .cowork-component {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: ${THEME.bg};
            color: ${THEME.text};
            border: 1px solid ${THEME.border};
            border-radius: 8px;
            overflow: hidden;
        }
        .cowork-toolbar {
            background: ${THEME.bgDark};
            padding: 8px 12px;
            border-bottom: 1px solid ${THEME.border};
            display: flex;
            gap: 8px;
            align-items: center;
        }
        .cowork-btn {
            background: ${THEME.bgLight};
            color: ${THEME.text};
            border: 1px solid ${THEME.border};
            border-radius: 4px;
            padding: 6px 10px;
            cursor: pointer;
            font-size: 12px;
            display: flex;
            align-items: center;
            gap: 4px;
        }
        .cowork-btn:hover {
            background: ${THEME.primary};
            color: ${THEME.bg};
        }
        .cowork-btn.active {
            background: ${THEME.secondary};
            color: ${THEME.bg};
        }
        .cowork-input {
            background: ${THEME.bgLight};
            color: ${THEME.text};
            border: 1px solid ${THEME.border};
            border-radius: 4px;
            padding: 6px 8px;
            font-size: 12px;
        }
        .cowork-input:focus {
            outline: none;
            border-color: ${THEME.primary};
        }
    `;

    // Inject styles
    if (!document.getElementById('cowork-styles')) {
        const style = document.createElement('style');
        style.id = 'cowork-styles';
        style.textContent = commonStyles;
        document.head.appendChild(style);
    }

    // Document Component
    class Document {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.data = options.data || '';
            this.onSave = options.onSave;
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const buttons = [
                { icon: 'B', cmd: 'bold', title: 'Bold' },
                { icon: 'I', cmd: 'italic', title: 'Italic' },
                { icon: 'H1', cmd: 'formatBlock', value: 'h1', title: 'Heading 1' },
                { icon: 'H2', cmd: 'formatBlock', value: 'h2', title: 'Heading 2' },
                { icon: 'UL', cmd: 'insertUnorderedList', title: 'Bullet List' },
                { icon: 'OL', cmd: 'insertOrderedList', title: 'Numbered List' },
                { icon: 'CODE', cmd: 'formatBlock', value: 'pre', title: 'Code Block' }
            ];

            buttons.forEach(btn => {
                const button = createElement('button', 'cowork-btn');
                button.textContent = btn.icon;
                button.title = btn.title;
                button.addEventListener('click', () => {
                    document.execCommand(btn.cmd, false, btn.value);
                    this.editor.focus();
                });
                toolbar.appendChild(button);
            });

            // Save button
            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.style.marginLeft = 'auto';
                saveBtn.addEventListener('click', () => this.save());
                toolbar.appendChild(saveBtn);
            }

            // Editor
            this.editor = createElement('div', '', {
                flex: '1',
                padding: '16px',
                outline: 'none',
                overflowY: 'auto',
                fontSize: '14px',
                lineHeight: '1.6'
            });
            this.editor.contentEditable = true;

            // Markdown shortcuts
            this.editor.addEventListener('input', (e) => {
                const selection = window.getSelection();
                const range = selection.getRangeAt(0);
                const line = range.startContainer.textContent;
                
                // Simple markdown shortcuts
                if (line.startsWith('# ') && range.startOffset > 2) {
                    document.execCommand('formatBlock', false, 'h1');
                    this.editor.innerHTML = this.editor.innerHTML.replace('# ', '');
                } else if (line.startsWith('## ') && range.startOffset > 3) {
                    document.execCommand('formatBlock', false, 'h2');
                    this.editor.innerHTML = this.editor.innerHTML.replace('## ', '');
                }
            });

            this.container.appendChild(toolbar);
            this.container.appendChild(this.editor);
        }

        async loadData() {
            if (this.data && this.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.data);
                    const content = await response.text();
                    this.setData(content);
                } catch (err) {
                    console.error('Failed to load document data:', err);
                }
            } else if (this.data) {
                this.setData(this.data);
            }
        }

        getData() {
            return this.editor.innerHTML;
        }

        setData(html) {
            this.editor.innerHTML = html;
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Document(container, options);
        }
    }

    // Sheet Component
    class Sheet {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.data = Array(100).fill().map(() => Array(26).fill(''));
            this.onSave = options.onSave;
            this.activeCell = { row: 0, col: 0 };
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const importBtn = createElement('button', 'cowork-btn');
            importBtn.textContent = 'Import CSV';
            importBtn.addEventListener('click', () => this.importCSV());
            toolbar.appendChild(importBtn);

            const exportBtn = createElement('button', 'cowork-btn');
            exportBtn.textContent = 'Export CSV';
            exportBtn.addEventListener('click', () => this.exportCSV());
            toolbar.appendChild(exportBtn);

            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.style.marginLeft = 'auto';
                saveBtn.addEventListener('click', () => this.save());
                toolbar.appendChild(saveBtn);
            }

            // Sheet container
            const sheetContainer = createElement('div', '', {
                flex: '1',
                overflow: 'auto',
                position: 'relative'
            });

            // Table
            this.table = createElement('table', '', {
                borderCollapse: 'collapse',
                fontSize: '12px',
                width: '100%'
            });

            // Header row
            const headerRow = createElement('tr');
            headerRow.appendChild(createElement('th', '', {
                background: THEME.bgDark,
                border: `1px solid ${THEME.border}`,
                padding: '4px 8px',
                minWidth: '40px'
            })); // Empty corner

            for (let col = 0; col < 26; col++) {
                const th = createElement('th', '', {
                    background: THEME.bgDark,
                    border: `1px solid ${THEME.border}`,
                    padding: '4px 8px',
                    minWidth: '80px'
                });
                th.textContent = String.fromCharCode(65 + col);
                headerRow.appendChild(th);
            }
            this.table.appendChild(headerRow);

            // Data rows
            for (let row = 0; row < 100; row++) {
                const tr = createElement('tr');
                
                // Row header
                const rowHeader = createElement('td', '', {
                    background: THEME.bgDark,
                    border: `1px solid ${THEME.border}`,
                    padding: '4px 8px',
                    textAlign: 'center',
                    fontWeight: 'bold'
                });
                rowHeader.textContent = row + 1;
                tr.appendChild(rowHeader);

                // Data cells
                for (let col = 0; col < 26; col++) {
                    const td = createElement('td', '', {
                        border: `1px solid ${THEME.border}`,
                        padding: '0',
                        position: 'relative'
                    });

                    const input = createElement('input', 'cowork-input', {
                        width: '100%',
                        height: '24px',
                        border: 'none',
                        background: 'transparent',
                        padding: '4px 8px',
                        fontSize: '12px'
                    });

                    input.value = this.data[row][col] || '';
                    input.addEventListener('focus', () => {
                        this.activeCell = { row, col };
                        input.style.background = THEME.bgLight;
                    });

                    input.addEventListener('blur', () => {
                        input.style.background = 'transparent';
                        this.data[row][col] = input.value;
                        this.updateFormulas();
                    });

                    input.addEventListener('keydown', (e) => {
                        if (e.key === 'Tab') {
                            e.preventDefault();
                            this.navigateCell(row, col + 1);
                        } else if (e.key === 'Enter') {
                            e.preventDefault();
                            this.navigateCell(row + 1, col);
                        }
                    });

                    td.appendChild(input);
                    tr.appendChild(td);
                }
                this.table.appendChild(tr);
            }

            sheetContainer.appendChild(this.table);
            this.container.appendChild(toolbar);
            this.container.appendChild(sheetContainer);
        }

        navigateCell(row, col) {
            if (row >= 0 && row < 100 && col >= 0 && col < 26) {
                const input = this.table.rows[row + 1].cells[col + 1].querySelector('input');
                input.focus();
            }
        }

        updateFormulas() {
            // Simple formula evaluation
            for (let row = 0; row < 100; row++) {
                for (let col = 0; col < 26; col++) {
                    const value = this.data[row][col];
                    if (value.startsWith('=')) {
                        const formula = value.substring(1).toUpperCase();
                        let result = this.evaluateFormula(formula);
                        const input = this.table.rows[row + 1].cells[col + 1].querySelector('input');
                        input.style.color = THEME.secondary;
                        if (result !== null) {
                            input.setAttribute('title', `Formula: ${value}`);
                        }
                    }
                }
            }
        }

        evaluateFormula(formula) {
            try {
                // Basic formulas: SUM(A1:A10), AVG(A1:A10), COUNT(A1:A10)
                const sumMatch = formula.match(/SUM\(([A-Z]\d+):([A-Z]\d+)\)/);
                if (sumMatch) {
                    const range = this.parseRange(sumMatch[1], sumMatch[2]);
                    return range.reduce((sum, val) => sum + (parseFloat(val) || 0), 0);
                }

                const avgMatch = formula.match(/AVG\(([A-Z]\d+):([A-Z]\d+)\)/);
                if (avgMatch) {
                    const range = this.parseRange(avgMatch[1], avgMatch[2]);
                    const numbers = range.filter(val => !isNaN(parseFloat(val)));
                    return numbers.length > 0 ? numbers.reduce((sum, val) => sum + parseFloat(val), 0) / numbers.length : 0;
                }

                const countMatch = formula.match(/COUNT\(([A-Z]\d+):([A-Z]\d+)\)/);
                if (countMatch) {
                    const range = this.parseRange(countMatch[1], countMatch[2]);
                    return range.filter(val => !isNaN(parseFloat(val)) && val !== '').length;
                }

                return null;
            } catch (e) {
                return null;
            }
        }

        parseRange(start, end) {
            const startCol = start.charCodeAt(0) - 65;
            const startRow = parseInt(start.substring(1)) - 1;
            const endCol = end.charCodeAt(0) - 65;
            const endRow = parseInt(end.substring(1)) - 1;

            const values = [];
            for (let row = startRow; row <= endRow; row++) {
                for (let col = startCol; col <= endCol; col++) {
                    if (row >= 0 && row < 100 && col >= 0 && col < 26) {
                        values.push(this.data[row][col] || '');
                    }
                }
            }
            return values;
        }

        async loadData() {
            if (this.options.data && this.options.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.options.data);
                    const csv = await response.text();
                    this.parseCSV(csv);
                    this.updateTable();
                } catch (err) {
                    console.error('Failed to load sheet data:', err);
                }
            }
        }

        parseCSV(csv) {
            const rows = csv.split('\n');
            for (let i = 0; i < rows.length && i < 100; i++) {
                const cells = rows[i].split(',');
                for (let j = 0; j < cells.length && j < 26; j++) {
                    this.data[i][j] = cells[j].trim();
                }
            }
        }

        updateTable() {
            for (let row = 0; row < 100; row++) {
                for (let col = 0; col < 26; col++) {
                    const input = this.table.rows[row + 1].cells[col + 1].querySelector('input');
                    input.value = this.data[row][col] || '';
                }
            }
            this.updateFormulas();
        }

        getData() {
            return this.data.map(row => row.join(',')).join('\n');
        }

        setData(csv) {
            this.parseCSV(csv);
            this.updateTable();
        }

        importCSV() {
            const input = document.createElement('input');
            input.type = 'file';
            input.accept = '.csv';
            input.onchange = (e) => {
                const file = e.target.files[0];
                if (file) {
                    const reader = new FileReader();
                    reader.onload = (e) => this.setData(e.target.result);
                    reader.readAsText(file);
                }
            };
            input.click();
        }

        exportCSV() {
            const csv = this.getData();
            const blob = new Blob([csv], { type: 'text/csv' });
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = 'sheet.csv';
            a.click();
            URL.revokeObjectURL(url);
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Sheet(container, options);
        }
    }

    // Canvas Component
    class Canvas {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.onSave = options.onSave;
            this.tool = 'pen';
            this.color = THEME.primary;
            this.shapes = [];
            this.history = [];
            this.historyIndex = -1;
            this.isDrawing = false;
            this.startPos = null;
            this.zoom = 1;
            this.pan = { x: 0, y: 0 };
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const tools = [
                { name: 'pen', icon: '✏️', title: 'Pen' },
                { name: 'rectangle', icon: '⬜', title: 'Rectangle' },
                { name: 'circle', icon: '⭕', title: 'Circle' },
                { name: 'text', icon: 'T', title: 'Text' },
                { name: 'arrow', icon: '↗️', title: 'Arrow' },
                { name: 'eraser', icon: '🧹', title: 'Eraser' }
            ];

            tools.forEach(tool => {
                const btn = createElement('button', 'cowork-btn');
                btn.innerHTML = tool.icon;
                btn.title = tool.title;
                btn.classList.toggle('active', this.tool === tool.name);
                btn.addEventListener('click', () => {
                    this.tool = tool.name;
                    toolbar.querySelectorAll('.cowork-btn').forEach(b => b.classList.remove('active'));
                    btn.classList.add('active');
                });
                toolbar.appendChild(btn);
            });

            // Color picker
            const colorInput = createElement('input', '', {
                type: 'color',
                value: this.color,
                width: '32px',
                height: '32px',
                border: 'none',
                borderRadius: '4px',
                cursor: 'pointer'
            });
            colorInput.addEventListener('change', (e) => this.color = e.target.value);
            toolbar.appendChild(colorInput);

            // Undo/Redo
            const undoBtn = createElement('button', 'cowork-btn');
            undoBtn.textContent = '↶';
            undoBtn.title = 'Undo';
            undoBtn.addEventListener('click', () => this.undo());
            toolbar.appendChild(undoBtn);

            const redoBtn = createElement('button', 'cowork-btn');
            redoBtn.textContent = '↷';
            redoBtn.title = 'Redo';
            redoBtn.addEventListener('click', () => this.redo());
            toolbar.appendChild(redoBtn);

            // Clear
            const clearBtn = createElement('button', 'cowork-btn');
            clearBtn.textContent = 'Clear';
            clearBtn.addEventListener('click', () => this.clear());
            toolbar.appendChild(clearBtn);

            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.style.marginLeft = 'auto';
                saveBtn.addEventListener('click', () => this.save());
                toolbar.appendChild(saveBtn);
            }

            // Canvas
            this.canvas = createElement('canvas', '', {
                flex: '1',
                cursor: 'crosshair'
            });
            this.ctx = this.canvas.getContext('2d');
            
            this.resizeCanvas();
            this.setupCanvasEvents();

            this.container.appendChild(toolbar);
            this.container.appendChild(this.canvas);

            // Handle resize
            window.addEventListener('resize', () => this.resizeCanvas());
        }

        resizeCanvas() {
            const rect = this.container.getBoundingClientRect();
            this.canvas.width = rect.width - 2; // Account for border
            this.canvas.height = rect.height - 60; // Account for toolbar
            this.redraw();
        }

        setupCanvasEvents() {
            this.canvas.addEventListener('mousedown', (e) => this.onMouseDown(e));
            this.canvas.addEventListener('mousemove', (e) => this.onMouseMove(e));
            this.canvas.addEventListener('mouseup', (e) => this.onMouseUp(e));
            this.canvas.addEventListener('wheel', (e) => this.onWheel(e));
            this.canvas.addEventListener('contextmenu', (e) => e.preventDefault());
        }

        getMousePos(e) {
            const rect = this.canvas.getBoundingClientRect();
            return {
                x: (e.clientX - rect.left - this.pan.x) / this.zoom,
                y: (e.clientY - rect.top - this.pan.y) / this.zoom
            };
        }

        onMouseDown(e) {
            if (e.button === 2) { // Right click for pan
                this.isPanning = true;
                this.lastPanPoint = { x: e.clientX, y: e.clientY };
                this.canvas.style.cursor = 'grab';
                return;
            }

            this.isDrawing = true;
            this.startPos = this.getMousePos(e);

            if (this.tool === 'pen' || this.tool === 'eraser') {
                this.currentStroke = {
                    type: this.tool,
                    points: [this.startPos],
                    color: this.color,
                    size: this.tool === 'eraser' ? 10 : 2
                };
            }
        }

        onMouseMove(e) {
            if (this.isPanning) {
                const dx = e.clientX - this.lastPanPoint.x;
                const dy = e.clientY - this.lastPanPoint.y;
                this.pan.x += dx;
                this.pan.y += dy;
                this.lastPanPoint = { x: e.clientX, y: e.clientY };
                this.redraw();
                return;
            }

            if (!this.isDrawing) return;

            const currentPos = this.getMousePos(e);

            if (this.tool === 'pen' || this.tool === 'eraser') {
                this.currentStroke.points.push(currentPos);
                this.redraw();
                this.drawStroke(this.currentStroke);
            } else {
                this.redraw();
                this.drawPreview(this.startPos, currentPos);
            }
        }

        onMouseUp(e) {
            if (this.isPanning) {
                this.isPanning = false;
                this.canvas.style.cursor = 'crosshair';
                return;
            }

            if (!this.isDrawing) return;

            const endPos = this.getMousePos(e);

            if (this.tool === 'pen' || this.tool === 'eraser') {
                this.addShape(this.currentStroke);
            } else if (this.tool === 'text') {
                const text = prompt('Enter text:');
                if (text) {
                    this.addShape({
                        type: 'text',
                        x: this.startPos.x,
                        y: this.startPos.y,
                        text: text,
                        color: this.color,
                        size: 16
                    });
                }
            } else {
                this.addShape({
                    type: this.tool,
                    x1: this.startPos.x,
                    y1: this.startPos.y,
                    x2: endPos.x,
                    y2: endPos.y,
                    color: this.color,
                    size: 2
                });
            }

            this.isDrawing = false;
            this.redraw();
        }

        onWheel(e) {
            e.preventDefault();
            const delta = e.deltaY > 0 ? 0.9 : 1.1;
            this.zoom *= delta;
            this.zoom = Math.max(0.1, Math.min(5, this.zoom));
            this.redraw();
        }

        addShape(shape) {
            this.shapes.push(shape);
            this.saveState();
        }

        drawStroke(stroke) {
            this.ctx.save();
            this.ctx.scale(this.zoom, this.zoom);
            this.ctx.translate(this.pan.x / this.zoom, this.pan.y / this.zoom);
            
            this.ctx.strokeStyle = stroke.color;
            this.ctx.lineWidth = stroke.size;
            this.ctx.lineCap = 'round';
            this.ctx.lineJoin = 'round';
            
            if (stroke.type === 'eraser') {
                this.ctx.globalCompositeOperation = 'destination-out';
            } else {
                this.ctx.globalCompositeOperation = 'source-over';
            }

            this.ctx.beginPath();
            stroke.points.forEach((point, i) => {
                if (i === 0) this.ctx.moveTo(point.x, point.y);
                else this.ctx.lineTo(point.x, point.y);
            });
            this.ctx.stroke();
            
            this.ctx.restore();
        }

        drawShape(shape) {
            this.ctx.save();
            this.ctx.scale(this.zoom, this.zoom);
            this.ctx.translate(this.pan.x / this.zoom, this.pan.y / this.zoom);
            
            this.ctx.strokeStyle = shape.color;
            this.ctx.fillStyle = shape.color;
            this.ctx.lineWidth = shape.size || 2;

            switch (shape.type) {
                case 'rectangle':
                    this.ctx.strokeRect(shape.x1, shape.y1, shape.x2 - shape.x1, shape.y2 - shape.y1);
                    break;
                case 'circle':
                    const radius = Math.sqrt(Math.pow(shape.x2 - shape.x1, 2) + Math.pow(shape.y2 - shape.y1, 2));
                    this.ctx.beginPath();
                    this.ctx.arc(shape.x1, shape.y1, radius, 0, 2 * Math.PI);
                    this.ctx.stroke();
                    break;
                case 'arrow':
                    this.drawArrow(shape.x1, shape.y1, shape.x2, shape.y2);
                    break;
                case 'text':
                    this.ctx.font = `${shape.size}px sans-serif`;
                    this.ctx.fillText(shape.text, shape.x, shape.y);
                    break;
                case 'pen':
                case 'eraser':
                    this.drawStroke(shape);
                    return;
            }
            
            this.ctx.restore();
        }

        drawArrow(x1, y1, x2, y2) {
            const headlen = 10;
            const dx = x2 - x1;
            const dy = y2 - y1;
            const angle = Math.atan2(dy, dx);
            
            this.ctx.beginPath();
            this.ctx.moveTo(x1, y1);
            this.ctx.lineTo(x2, y2);
            this.ctx.lineTo(x2 - headlen * Math.cos(angle - Math.PI / 6), y2 - headlen * Math.sin(angle - Math.PI / 6));
            this.ctx.moveTo(x2, y2);
            this.ctx.lineTo(x2 - headlen * Math.cos(angle + Math.PI / 6), y2 - headlen * Math.sin(angle + Math.PI / 6));
            this.ctx.stroke();
        }

        drawPreview(start, end) {
            this.ctx.save();
            this.ctx.scale(this.zoom, this.zoom);
            this.ctx.translate(this.pan.x / this.zoom, this.pan.y / this.zoom);
            
            this.ctx.strokeStyle = this.color;
            this.ctx.lineWidth = 2;
            this.ctx.setLineDash([5, 5]);

            switch (this.tool) {
                case 'rectangle':
                    this.ctx.strokeRect(start.x, start.y, end.x - start.x, end.y - start.y);
                    break;
                case 'circle':
                    const radius = Math.sqrt(Math.pow(end.x - start.x, 2) + Math.pow(end.y - start.y, 2));
                    this.ctx.beginPath();
                    this.ctx.arc(start.x, start.y, radius, 0, 2 * Math.PI);
                    this.ctx.stroke();
                    break;
                case 'arrow':
                    this.drawArrow(start.x, start.y, end.x, end.y);
                    break;
            }
            
            this.ctx.restore();
        }

        redraw() {
            this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
            this.shapes.forEach(shape => this.drawShape(shape));
        }

        saveState() {
            this.history = this.history.slice(0, this.historyIndex + 1);
            this.history.push(JSON.parse(JSON.stringify(this.shapes)));
            this.historyIndex++;
        }

        undo() {
            if (this.historyIndex > 0) {
                this.historyIndex--;
                this.shapes = JSON.parse(JSON.stringify(this.history[this.historyIndex]));
                this.redraw();
            }
        }

        redo() {
            if (this.historyIndex < this.history.length - 1) {
                this.historyIndex++;
                this.shapes = JSON.parse(JSON.stringify(this.history[this.historyIndex]));
                this.redraw();
            }
        }

        clear() {
            this.shapes = [];
            this.saveState();
            this.redraw();
        }

        async loadData() {
            if (this.options.data && this.options.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.options.data);
                    const data = await response.json();
                    this.setData(data);
                } catch (err) {
                    console.error('Failed to load canvas data:', err);
                }
            }
        }

        getData() {
            return JSON.stringify(this.shapes);
        }

        setData(data) {
            try {
                this.shapes = typeof data === 'string' ? JSON.parse(data) : data;
                this.saveState();
                this.redraw();
            } catch (err) {
                console.error('Invalid canvas data:', err);
            }
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Canvas(container, options);
        }
    }

    // Board Component
    class Board {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.onSave = options.onSave;
            this.data = {
                columns: [
                    { id: 'todo', title: 'Todo', cards: [] },
                    { id: 'progress', title: 'In Progress', cards: [] },
                    { id: 'done', title: 'Done', cards: [] }
                ]
            };
            this.draggedCard = null;
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const addCardBtn = createElement('button', 'cowork-btn');
            addCardBtn.textContent = '+ Add Card';
            addCardBtn.addEventListener('click', () => this.addCard());
            toolbar.appendChild(addCardBtn);

            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.style.marginLeft = 'auto';
                saveBtn.addEventListener('click', () => this.save());
                toolbar.appendChild(saveBtn);
            }

            // Board
            this.board = createElement('div', '', {
                flex: '1',
                display: 'flex',
                gap: '16px',
                padding: '16px',
                overflowX: 'auto'
            });

            this.renderColumns();

            this.container.appendChild(toolbar);
            this.container.appendChild(this.board);
        }

        renderColumns() {
            this.board.innerHTML = '';
            
            this.data.columns.forEach(column => {
                const columnEl = createElement('div', '', {
                    minWidth: '250px',
                    background: THEME.bgLight,
                    borderRadius: '8px',
                    padding: '12px',
                    border: `1px solid ${THEME.border}`
                });

                // Column header
                const header = createElement('div', '', {
                    fontWeight: 'bold',
                    marginBottom: '12px',
                    padding: '8px',
                    background: THEME.bgDark,
                    borderRadius: '4px',
                    textAlign: 'center'
                });
                header.textContent = column.title;
                columnEl.appendChild(header);

                // Cards container
                const cardsContainer = createElement('div', '', {
                    minHeight: '200px'
                });
                cardsContainer.setAttribute('data-column', column.id);

                // Drop zone
                cardsContainer.addEventListener('dragover', (e) => e.preventDefault());
                cardsContainer.addEventListener('drop', (e) => this.onDrop(e, column.id));

                column.cards.forEach((card, index) => {
                    const cardEl = this.createCardElement(card, column.id, index);
                    cardsContainer.appendChild(cardEl);
                });

                columnEl.appendChild(cardsContainer);
                this.board.appendChild(columnEl);
            });
        }

        createCardElement(card, columnId, index) {
            const cardEl = createElement('div', '', {
                background: THEME.bg,
                border: `1px solid ${THEME.border}`,
                borderRadius: '6px',
                padding: '12px',
                marginBottom: '8px',
                cursor: 'move'
            });

            cardEl.draggable = true;
            cardEl.textContent = card.text;
            cardEl.setAttribute('data-card-id', card.id);

            // Drag events
            cardEl.addEventListener('dragstart', (e) => {
                this.draggedCard = { card, columnId, index };
                e.dataTransfer.effectAllowed = 'move';
            });

            // Double-click to edit
            cardEl.addEventListener('dblclick', () => {
                const newText = prompt('Edit card:', card.text);
                if (newText !== null) {
                    card.text = newText;
                    this.renderColumns();
                }
            });

            return cardEl;
        }

        onDrop(e, targetColumnId) {
            e.preventDefault();
            
            if (!this.draggedCard) return;

            const { card, columnId: sourceColumnId, index: sourceIndex } = this.draggedCard;

            // Remove from source
            const sourceColumn = this.data.columns.find(col => col.id === sourceColumnId);
            sourceColumn.cards.splice(sourceIndex, 1);

            // Add to target
            const targetColumn = this.data.columns.find(col => col.id === targetColumnId);
            targetColumn.cards.push(card);

            this.draggedCard = null;
            this.renderColumns();
        }

        addCard() {
            const text = prompt('Enter card text:');
            if (text) {
                const card = {
                    id: Date.now().toString(),
                    text: text,
                    createdAt: new Date().toISOString()
                };
                
                // Add to first column
                this.data.columns[0].cards.push(card);
                this.renderColumns();
            }
        }

        async loadData() {
            if (this.options.data && this.options.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.options.data);
                    const data = await response.json();
                    this.setData(data);
                } catch (err) {
                    console.error('Failed to load board data:', err);
                }
            }
        }

        getData() {
            return JSON.stringify(this.data);
        }

        setData(data) {
            try {
                this.data = typeof data === 'string' ? JSON.parse(data) : data;
                this.renderColumns();
            } catch (err) {
                console.error('Invalid board data:', err);
            }
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Board(container, options);
        }
    }

    // Calendar Component
    class Calendar {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.onSave = options.onSave;
            this.currentDate = new Date();
            this.events = [];
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            // Header
            const header = createElement('div', 'cowork-toolbar');
            
            const prevBtn = createElement('button', 'cowork-btn');
            prevBtn.innerHTML = '←';
            prevBtn.addEventListener('click', () => this.previousMonth());
            header.appendChild(prevBtn);

            this.monthYear = createElement('div', '', {
                flex: '1',
                textAlign: 'center',
                fontWeight: 'bold'
            });
            header.appendChild(this.monthYear);

            const nextBtn = createElement('button', 'cowork-btn');
            nextBtn.innerHTML = '→';
            nextBtn.addEventListener('click', () => this.nextMonth());
            header.appendChild(nextBtn);

            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.addEventListener('click', () => this.save());
                header.appendChild(saveBtn);
            }

            // Calendar grid
            this.calendarGrid = createElement('div', '', {
                flex: '1',
                display: 'grid',
                gridTemplateColumns: 'repeat(7, 1fr)',
                gap: '1px',
                background: THEME.border,
                margin: '0',
                overflow: 'hidden'
            });

            this.renderCalendar();

            this.container.appendChild(header);
            this.container.appendChild(this.calendarGrid);
        }

        renderCalendar() {
            this.monthYear.textContent = this.currentDate.toLocaleDateString('en-US', { 
                month: 'long', 
                year: 'numeric' 
            });

            this.calendarGrid.innerHTML = '';

            // Day headers
            const dayNames = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
            dayNames.forEach(day => {
                const dayHeader = createElement('div', '', {
                    background: THEME.bgDark,
                    padding: '8px',
                    textAlign: 'center',
                    fontWeight: 'bold',
                    fontSize: '12px'
                });
                dayHeader.textContent = day;
                this.calendarGrid.appendChild(dayHeader);
            });

            // Calendar days
            const firstDay = new Date(this.currentDate.getFullYear(), this.currentDate.getMonth(), 1);
            const lastDay = new Date(this.currentDate.getFullYear(), this.currentDate.getMonth() + 1, 0);
            const startDate = new Date(firstDay);
            startDate.setDate(startDate.getDate() - firstDay.getDay());

            for (let i = 0; i < 42; i++) { // 6 weeks
                const date = new Date(startDate);
                date.setDate(date.getDate() + i);
                
                const dayEl = createElement('div', '', {
                    background: THEME.bg,
                    padding: '4px',
                    minHeight: '60px',
                    cursor: 'pointer',
                    position: 'relative',
                    border: `1px solid ${THEME.border}`
                });

                if (date.getMonth() !== this.currentDate.getMonth()) {
                    dayEl.style.opacity = '0.3';
                }

                const dayNumber = createElement('div', '', {
                    fontSize: '12px',
                    fontWeight: 'bold',
                    marginBottom: '2px'
                });
                dayNumber.textContent = date.getDate();
                dayEl.appendChild(dayNumber);

                // Events for this date
                const dayEvents = this.getEventsForDate(date);
                dayEvents.forEach(event => {
                    const eventEl = createElement('div', '', {
                        background: THEME.secondary,
                        color: THEME.bg,
                        fontSize: '10px',
                        padding: '1px 4px',
                        marginBottom: '1px',
                        borderRadius: '2px',
                        overflow: 'hidden',
                        whiteSpace: 'nowrap',
                        textOverflow: 'ellipsis'
                    });
                    eventEl.textContent = event.title;
                    eventEl.title = event.title;
                    dayEl.appendChild(eventEl);
                });

                dayEl.addEventListener('click', () => this.addEvent(date));

                this.calendarGrid.appendChild(dayEl);
            }
        }

        getEventsForDate(date) {
            const dateStr = date.toISOString().split('T')[0];
            return this.events.filter(event => event.date === dateStr);
        }

        addEvent(date) {
            const title = prompt('Event title:');
            if (title) {
                const event = {
                    id: Date.now().toString(),
                    title: title,
                    date: date.toISOString().split('T')[0],
                    createdAt: new Date().toISOString()
                };
                this.events.push(event);
                this.renderCalendar();
            }
        }

        previousMonth() {
            this.currentDate.setMonth(this.currentDate.getMonth() - 1);
            this.renderCalendar();
        }

        nextMonth() {
            this.currentDate.setMonth(this.currentDate.getMonth() + 1);
            this.renderCalendar();
        }

        async loadData() {
            if (this.options.data && this.options.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.options.data);
                    const data = await response.json();
                    this.setData(data);
                } catch (err) {
                    console.error('Failed to load calendar data:', err);
                }
            }
        }

        getData() {
            return JSON.stringify(this.events);
        }

        setData(data) {
            try {
                this.events = typeof data === 'string' ? JSON.parse(data) : data;
                this.renderCalendar();
            } catch (err) {
                console.error('Invalid calendar data:', err);
            }
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Calendar(container, options);
        }
    }

    // Form Component
    class Form {
        constructor(container, options = {}) {
            this.container = typeof container === 'string' ? document.querySelector(container) : container;
            this.options = options;
            this.onSave = options.onSave;
            this.mode = options.mode || 'edit'; // 'edit' or 'fill'
            this.fields = [];
            this.draggedField = null;
            this.render();
            this.loadData();
        }

        render() {
            this.container.innerHTML = '';
            this.container.className = 'cowork-component';
            this.container.style.height = '100%';
            this.container.style.display = 'flex';
            this.container.style.flexDirection = 'column';

            if (this.mode === 'edit') {
                this.renderEditMode();
            } else {
                this.renderFillMode();
            }
        }

        renderEditMode() {
            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const fieldTypes = [
                { type: 'text', label: 'Text', icon: '📝' },
                { type: 'number', label: 'Number', icon: '#' },
                { type: 'email', label: 'Email', icon: '📧' },
                { type: 'date', label: 'Date', icon: '📅' },
                { type: 'textarea', label: 'Text Area', icon: '📄' },
                { type: 'select', label: 'Select', icon: '📋' },
                { type: 'checkbox', label: 'Checkbox', icon: '☑️' },
                { type: 'radio', label: 'Radio', icon: '⚪' }
            ];

            fieldTypes.forEach(fieldType => {
                const btn = createElement('button', 'cowork-btn');
                btn.innerHTML = `${fieldType.icon} ${fieldType.label}`;
                btn.addEventListener('click', () => this.addField(fieldType.type));
                toolbar.appendChild(btn);
            });

            const modeBtn = createElement('button', 'cowork-btn');
            modeBtn.textContent = 'Preview';
            modeBtn.style.marginLeft = 'auto';
            modeBtn.addEventListener('click', () => {
                this.mode = 'fill';
                this.render();
            });
            toolbar.appendChild(modeBtn);

            if (this.onSave) {
                const saveBtn = createElement('button', 'cowork-btn');
                saveBtn.textContent = 'Save';
                saveBtn.addEventListener('click', () => this.save());
                toolbar.appendChild(saveBtn);
            }

            // Form builder
            this.formBuilder = createElement('div', '', {
                flex: '1',
                padding: '16px',
                overflowY: 'auto'
            });

            this.renderFields();

            this.container.appendChild(toolbar);
            this.container.appendChild(this.formBuilder);
        }

        renderFillMode() {
            // Toolbar
            const toolbar = createElement('div', 'cowork-toolbar');
            
            const editBtn = createElement('button', 'cowork-btn');
            editBtn.textContent = 'Edit Form';
            editBtn.addEventListener('click', () => {
                this.mode = 'edit';
                this.render();
            });
            toolbar.appendChild(editBtn);

            const submitBtn = createElement('button', 'cowork-btn');
            submitBtn.textContent = 'Submit';
            submitBtn.style.marginLeft = 'auto';
            submitBtn.addEventListener('click', () => this.submitForm());
            toolbar.appendChild(submitBtn);

            // Form
            this.form = createElement('form', '', {
                flex: '1',
                padding: '16px',
                overflowY: 'auto'
            });

            this.renderFillableForm();

            this.container.appendChild(toolbar);
            this.container.appendChild(this.form);
        }

        renderFields() {
            this.formBuilder.innerHTML = '';

            if (this.fields.length === 0) {
                const placeholder = createElement('div', '', {
                    textAlign: 'center',
                    color: THEME.textMuted,
                    padding: '40px',
                    fontStyle: 'italic'
                });
                placeholder.textContent = 'Drag and drop fields from the toolbar to build your form';
                this.formBuilder.appendChild(placeholder);
                return;
            }

            this.fields.forEach((field, index) => {
                const fieldEl = this.createFieldEditor(field, index);
                this.formBuilder.appendChild(fieldEl);
            });
        }

        createFieldEditor(field, index) {
            const fieldEl = createElement('div', '', {
                background: THEME.bgLight,
                border: `1px solid ${THEME.border}`,
                borderRadius: '6px',
                padding: '12px',
                marginBottom: '8px',
                position: 'relative'
            });

            // Field controls
            const controls = createElement('div', '', {
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
                marginBottom: '8px'
            });

            const label = createElement('input', 'cowork-input', {
                flex: '1',
                marginRight: '8px'
            });
            label.placeholder = 'Field Label';
            label.value = field.label || '';
            label.addEventListener('input', () => field.label = label.value);

            const deleteBtn = createElement('button', 'cowork-btn', {
                background: THEME.danger
            });
            deleteBtn.textContent = '×';
            deleteBtn.addEventListener('click', () => this.deleteField(index));

            controls.appendChild(label);
            controls.appendChild(deleteBtn);

            // Field preview
            const preview = createElement('div', '', {
                border: `1px dashed ${THEME.border}`,
                borderRadius: '4px',
                padding: '8px'
            });

            const previewField = this.createPreviewField(field);
            preview.appendChild(previewField);

            // Field options
            const options = this.createFieldOptions(field);

            fieldEl.appendChild(controls);
            fieldEl.appendChild(preview);
            if (options) fieldEl.appendChild(options);

            return fieldEl;
        }

        createPreviewField(field) {
            const wrapper = createElement('div');
            
            if (field.label) {
                const label = createElement('label', '', {
                    display: 'block',
                    marginBottom: '4px',
                    fontWeight: 'bold',
                    fontSize: '12px'
                });
                label.textContent = field.label;
                wrapper.appendChild(label);
            }

            let input;
            switch (field.type) {
                case 'textarea':
                    input = createElement('textarea', 'cowork-input', {
                        width: '100%',
                        height: '60px',
                        resize: 'vertical'
                    });
                    break;
                case 'select':
                    input = createElement('select', 'cowork-input', { width: '100%' });
                    (field.options || ['Option 1', 'Option 2']).forEach(option => {
                        const optionEl = createElement('option');
                        optionEl.textContent = option;
                        input.appendChild(optionEl);
                    });
                    break;
                case 'checkbox':
                case 'radio':
                    input = createElement('div');
                    (field.options || ['Option 1', 'Option 2']).forEach(option => {
                        const wrapper = createElement('label', '', {
                            display: 'block',
                            marginBottom: '4px'
                        });
                        const inputEl = createElement('input', '', {
                            type: field.type,
                            marginRight: '8px'
                        });
                        if (field.type === 'radio') {
                            inputEl.name = `field_${Date.now()}`;
                        }
                        wrapper.appendChild(inputEl);
                        wrapper.appendChild(document.createTextNode(option));
                        input.appendChild(wrapper);
                    });
                    break;
                default:
                    input = createElement('input', 'cowork-input', {
                        type: field.type,
                        width: '100%'
                    });
                    if (field.placeholder) input.placeholder = field.placeholder;
            }

            wrapper.appendChild(input);
            return wrapper;
        }

        createFieldOptions(field) {
            if (!['select', 'checkbox', 'radio'].includes(field.type)) {
                if (field.type === 'text' || field.type === 'textarea') {
                    const placeholderInput = createElement('input', 'cowork-input', {
                        width: '100%',
                        marginTop: '8px'
                    });
                    placeholderInput.placeholder = 'Placeholder text';
                    placeholderInput.value = field.placeholder || '';
                    placeholderInput.addEventListener('input', () => field.placeholder = placeholderInput.value);
                    return placeholderInput;
                }
                return null;
            }

            const optionsEl = createElement('div', '', { marginTop: '8px' });
            
            const title = createElement('div', '', {
                fontSize: '12px',
                fontWeight: 'bold',
                marginBottom: '4px'
            });
            title.textContent = 'Options:';
            optionsEl.appendChild(title);

            field.options = field.options || ['Option 1', 'Option 2'];
            
            field.options.forEach((option, index) => {
                const optionEl = createElement('div', '', {
                    display: 'flex',
                    marginBottom: '4px'
                });

                const input = createElement('input', 'cowork-input', {
                    flex: '1',
                    marginRight: '8px'
                });
                input.value = option;
                input.addEventListener('input', () => field.options[index] = input.value);

                const deleteBtn = createElement('button', 'cowork-btn');
                deleteBtn.textContent = '×';
                deleteBtn.addEventListener('click', () => {
                    field.options.splice(index, 1);
                    this.renderFields();
                });

                optionEl.appendChild(input);
                optionEl.appendChild(deleteBtn);
                optionsEl.appendChild(optionEl);
            });

            const addBtn = createElement('button', 'cowork-btn');
            addBtn.textContent = '+ Add Option';
            addBtn.addEventListener('click', () => {
                field.options.push(`Option ${field.options.length + 1}`);
                this.renderFields();
            });
            optionsEl.appendChild(addBtn);

            return optionsEl;
        }

        renderFillableForm() {
            this.form.innerHTML = '';

            this.fields.forEach((field, index) => {
                const fieldEl = createElement('div', '', {
                    marginBottom: '16px'
                });

                const fieldInput = this.createPreviewField(field);
                fieldInput.querySelector('input, textarea, select').name = `field_${index}`;
                
                fieldEl.appendChild(fieldInput);
                this.form.appendChild(fieldEl);
            });
        }

        addField(type) {
            const field = {
                id: Date.now().toString(),
                type: type,
                label: `New ${type} field`,
                required: false
            };

            this.fields.push(field);
            this.renderFields();
        }

        deleteField(index) {
            this.fields.splice(index, 1);
            this.renderFields();
        }

        submitForm() {
            const formData = new FormData(this.form);
            const data = {};
            
            for (let [key, value] of formData.entries()) {
                data[key] = value;
            }

            if (this.onSave) {
                this.onSave({ fields: this.fields, submission: data });
            } else {
                alert('Form submitted:\n' + JSON.stringify(data, null, 2));
            }
        }

        async loadData() {
            if (this.options.data && this.options.data.startsWith('/api/file')) {
                try {
                    const response = await fetch(this.options.data);
                    const data = await response.json();
                    this.setData(data);
                } catch (err) {
                    console.error('Failed to load form data:', err);
                }
            }
        }

        getData() {
            return JSON.stringify({ fields: this.fields });
        }

        setData(data) {
            try {
                const parsed = typeof data === 'string' ? JSON.parse(data) : data;
                this.fields = parsed.fields || [];
                if (this.mode === 'edit') {
                    this.renderFields();
                } else {
                    this.renderFillableForm();
                }
            } catch (err) {
                console.error('Invalid form data:', err);
            }
        }

        save() {
            if (this.onSave) {
                this.onSave(this.getData());
            }
        }

        static create(container, options) {
            return new Form(container, options);
        }
    }

    // Export global Cowork object
    global.Cowork = {
        Document,
        Sheet,
        Canvas,
        Board,
        Calendar,
        Form
    };

})(typeof window !== 'undefined' ? window : global);
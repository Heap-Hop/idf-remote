// SPDX-License-Identifier: Apache-2.0
#include "idf_remote.h"
#include "mux_codec.h"
#include "driver/usb_serial_jtag.h"
#include "esp_random.h"
#include "esp_vfs.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/queue.h"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/stat.h>

static QueueHandle_t control_queue, console_queue, input_queue, request_queue;
static TaskHandle_t tx_handle;
static portMUX_TYPE state_lock=portMUX_INITIALIZER_UNLOCKED;
static uint64_t session;
static uint32_t dropped_console, dropped_control;
static idf_remote_config_t config;
static char *identity;
static bool attached;
static int input_flags;
static uint64_t current_session(void) {
    taskENTER_CRITICAL(&state_lock); uint64_t s=session; taskEXIT_CRITICAL(&state_lock); return s;
}
static void count_drop(bool control) {
    taskENTER_CRITICAL(&state_lock);
    if(control) dropped_control++; else dropped_console++;
    taskEXIT_CRITICAL(&state_lock);
}
uint32_t idf_remote_dropped_console(void) {
    taskENTER_CRITICAL(&state_lock); uint32_t n=dropped_console; taskEXIT_CRITICAL(&state_lock); return n;
}
uint32_t idf_remote_dropped_control(void) {
    taskENTER_CRITICAL(&state_lock); uint32_t n=dropped_control; taskEXIT_CRITICAL(&state_lock); return n;
}
static esp_err_t enqueue(mux_frame_t *frame, bool control) {
    if(xQueueSend(control ? control_queue : console_queue, frame, 0)!=pdTRUE) {
        count_drop(control); return ESP_ERR_TIMEOUT;
    }
    xTaskNotifyGive(tx_handle); return ESP_OK;
}
static esp_err_t send_json(uint8_t kind, uint64_t s, uint32_t id, const cJSON *value) {
    char *text=cJSON_PrintUnformatted(value);
    if(!text) return ESP_ERR_NO_MEM;
    mux_frame_t frame={.kind=kind,.session=s,.request=id,.size=strlen(text)};
    esp_err_t result=ESP_ERR_INVALID_SIZE;
    if(frame.size<=MUX_MAX_PAYLOAD) { memcpy(frame.data,text,frame.size); result=enqueue(&frame,true); }
    free(text); return result;
}
esp_err_t idf_remote_publish(const cJSON *event) {
    uint64_t s=current_session();
    if(!s) return ESP_ERR_INVALID_STATE;
    return send_json(MUX_EVENT,s,0,event);
}
static void transmit(const uint8_t *bytes, size_t size) {
    size_t sent=0;
    while(sent<size) {
        int n=usb_serial_jtag_write_bytes(bytes+sent,size-sent,pdMS_TO_TICKS(20));
        if(n>0) sent+=n;
        else { count_drop(true); break; }
    }
}
static void tx_task(void *arg) {
    mux_frame_t frame;
    uint8_t wire[MUX_MAX_WIRE];
    while(1) {
        if(xQueueReceive(control_queue,&frame,0)!=pdTRUE && xQueueReceive(console_queue,&frame,0)!=pdTRUE) {
            ulTaskNotifyTake(pdTRUE,portMAX_DELAY); continue;
        }
        uint64_t s=current_session();
        if(frame.kind==MUX_CONSOLE) {
            if(!s) { transmit(frame.data,frame.size); continue; }
            frame.session=s;
        } else if(frame.session!=s) continue;
        size_t size=mux_encode(&frame,wire);
        transmit(wire,size);
    }
}
static ssize_t console_write(int fd, const void *data, size_t size) {
    const uint8_t *bytes=data;
    // Small frames bound how long a log can hold the wire ahead of control.
    for(size_t offset=0;offset<size;) {
        mux_frame_t frame={.kind=MUX_CONSOLE,.size=(size-offset)>192 ? 192 : size-offset};
        memcpy(frame.data,bytes+offset,frame.size); enqueue(&frame,false); offset+=frame.size;
    }
    return size;
}
static ssize_t console_read(int fd, void *data, size_t size) {
    if(!size) return 0;
    uint8_t *bytes=data;
    if(xQueueReceive(input_queue,bytes,(input_flags&O_NONBLOCK)?0:portMAX_DELAY)!=pdTRUE) { errno=EAGAIN; return -1; }
    size_t n=1;
    while(n<size && xQueueReceive(input_queue,bytes+n,0)==pdTRUE) n++;
    return n;
}
static int console_open(const char *path, int flags, int mode) { return flags & O_ACCMODE; }
static int console_close(int fd) { return 0; }
static int console_stat(int fd, struct stat *s) { memset(s,0,sizeof(*s)); s->st_mode=S_IFCHR; return 0; }
static int console_fcntl(int fd, int cmd, int arg) {
    if(cmd==F_GETFL) return input_flags;
    if(cmd==F_SETFL) { input_flags=arg; return 0; }
    errno=ENOSYS; return -1;
}
static void protocol_error(uint64_t s, uint32_t id, const char *message) {
    cJSON *error=cJSON_CreateObject(); cJSON_AddStringToObject(error,"error",message);
    send_json(MUX_ERROR,s,id,error); cJSON_Delete(error);
}
static void command_task(void *arg) {
    mux_frame_t frame;
    while(xQueueReceive(request_queue,&frame,portMAX_DELAY)==pdTRUE) {
        if(frame.session!=current_session()) continue;
        char text[MUX_MAX_PAYLOAD+1]; memcpy(text,frame.data,frame.size); text[frame.size]=0;
        const char *end=NULL;
        cJSON *request=cJSON_ParseWithOpts(text,&end,true), *result=NULL;
        const cJSON *method=cJSON_GetObjectItemCaseSensitive(request,"method");
        const cJSON *params=cJSON_GetObjectItemCaseSensitive(request,"params");
        if(!cJSON_IsString(method)) { protocol_error(frame.session,frame.request,"invalid request"); }
        else {
            esp_err_t err=config.handler(method->valuestring,params,&result,config.context);
            if(err!=ESP_OK) protocol_error(frame.session,frame.request,esp_err_to_name(err));
            else {
                if(!result) result=cJSON_CreateNull();
                err=send_json(MUX_RESPONSE,frame.session,frame.request,result);
                if(err==ESP_ERR_INVALID_SIZE) protocol_error(frame.session,frame.request,"response exceeds 1024 bytes");
            }
        }
        cJSON_Delete(result); cJSON_Delete(request);
    }
}
static void rx_task(void *arg) {
    mux_decoder_t decoder={0}; mux_frame_t frame; uint8_t bytes[256];
    while(1) {
        int n=usb_serial_jtag_read_bytes(bytes,sizeof(bytes),pdMS_TO_TICKS(10));
        for(int i=0;i<n;i++) {
            int decoded=mux_feed(&decoder,bytes[i],&frame);
            if(decoded==2 && !current_session()) { xQueueSend(input_queue,&bytes[i],0); continue; }
            if(decoded!=1) continue;
            if(frame.kind==MUX_HELLO && !frame.request && !frame.size) {
                taskENTER_CRITICAL(&state_lock); session=frame.session; taskEXIT_CRITICAL(&state_lock);
                xQueueReset(input_queue);
                frame.kind=MUX_HELLO_ACK; frame.size=strlen(identity); memcpy(frame.data,identity,frame.size);
                enqueue(&frame,true);
            } else if(frame.session==current_session()) {
                if(frame.kind==MUX_CONSOLE && !frame.request) {
                    for(size_t j=0;j<frame.size;j++) if(xQueueSend(input_queue,frame.data+j,0)!=pdTRUE) count_drop(true);
                } else if(frame.kind==MUX_REQUEST && frame.request) {
                    if(xQueueSend(request_queue,&frame,0)!=pdTRUE) protocol_error(frame.session,frame.request,"command queue full");
                }
            }
        }
    }
}
esp_err_t idf_remote_attach_console(const idf_remote_config_t *cfg) {
    if(attached) return ESP_ERR_INVALID_STATE;
    if(!cfg || !cfg->application || !cfg->version || !cfg->methods_json || !cfg->handler) return ESP_ERR_INVALID_ARG;
    cJSON *hello=cJSON_CreateObject(), *methods=cJSON_Parse(cfg->methods_json);
    if(!hello || !cJSON_IsArray(methods)) { cJSON_Delete(hello); cJSON_Delete(methods); return ESP_ERR_INVALID_ARG; }
    cJSON_AddStringToObject(hello,"application",cfg->application);
    cJSON_AddStringToObject(hello,"version",cfg->version);
    cJSON_AddNumberToObject(hello,"protocol",1);
    cJSON_AddNumberToObject(hello,"boot_id",esp_random());
    cJSON_AddItemToObject(hello,"methods",methods);
    identity=cJSON_PrintUnformatted(hello); cJSON_Delete(hello);
    if(!identity || strlen(identity)>MUX_MAX_PAYLOAD) { free(identity); return ESP_ERR_INVALID_SIZE; }
    attached=true; config=*cfg;
    control_queue=xQueueCreate(8,sizeof(mux_frame_t)); console_queue=xQueueCreate(16,sizeof(mux_frame_t));
    request_queue=xQueueCreate(4,sizeof(mux_frame_t)); input_queue=xQueueCreate(1024,1);
    if(!control_queue || !console_queue || !request_queue || !input_queue) return ESP_ERR_NO_MEM;
    usb_serial_jtag_driver_config_t usb={.tx_buffer_size=256,.rx_buffer_size=2048};
    esp_err_t err=usb_serial_jtag_driver_install(&usb); if(err!=ESP_OK) return err;
    if(xTaskCreate(tx_task,"remote_tx",6144,NULL,10,&tx_handle)!=pdPASS) return ESP_ERR_NO_MEM;
    const esp_vfs_t vfs={.flags=ESP_VFS_FLAG_DEFAULT,.open=console_open,.close=console_close,
        .write=console_write,.read=console_read,.fstat=console_stat,.fcntl=console_fcntl};
    err=esp_vfs_register("/dev/idfremote",&vfs,NULL); if(err!=ESP_OK) return err;
    // freopen preserves shared FILE object addresses: existing default streams
    // and future tasks use the same VFS. Do this before application tasks start.
    fflush(stdout); fflush(stderr);
    if(!freopen("/dev/idfremote","r",stdin) || !freopen("/dev/idfremote","w",stdout) ||
       !freopen("/dev/idfremote","w",stderr)) return ESP_FAIL;
    setvbuf(stdin,NULL,_IONBF,0); setvbuf(stdout,NULL,_IONBF,0); setvbuf(stderr,NULL,_IONBF,0);
    if(xTaskCreate(command_task,"remote_cmd",6144,NULL,5,NULL)!=pdPASS ||
       xTaskCreate(rx_task,"remote_rx",6144,NULL,11,NULL)!=pdPASS) return ESP_ERR_NO_MEM;
    return ESP_OK;
}
